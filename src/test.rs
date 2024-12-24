use core::{error::Error, fmt, fmt::Display};
use std::{
    borrow::Borrow,
    collections::HashMap,
    ffi::{OsStr, OsString},
    fmt::{Debug, Formatter},
    path::Path,
    pin::pin,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, Context};
use futures::future::{self, select_all, try_join_all, Either, FutureExt};
use itertools::Itertools;
#[allow(unused_imports)]
use log::{debug, error, info, warn};
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{
    process::{Child, Command},
    select,
    sync::{broadcast, watch},
    time::sleep,
};
use tokio_util::sync::CancellationToken;

use crate::{
    dag::{Dag, GraphNode},
    database::{Database, DatabaseEntry, DatabaseOutput, LookupResult},
    git::{Commit, CommitHash, Hash, Worktree},
    process::ExitStatusExt as _,
    resource::{Pools, ResourceKey, Resources},
    util::ResultExt,
};

#[derive(Deserialize, JsonSchema, Serialize, Debug, Clone, Copy, Hash, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CachePolicy {
    NoCaching,
    ByCommit,
    ByTree,
}

impl CachePolicy {
    // Figure out the hash that should be used to store a result in the
    // database, if it should be stored at all
    pub fn cache_hash(&self, commit: &Commit) -> Option<Hash> {
        match self {
            CachePolicy::NoCaching => None::<Hash>,
            CachePolicy::ByCommit => Some(commit.hash.clone().into()),
            CachePolicy::ByTree => Some(commit.tree.clone().into()),
        }
    }
}

// Some unspecified hash, don't care too much about stability across builds.
pub type ConfigHash = String;

#[derive(Clone, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct TestName(String);

impl TestName {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl AsRef<Path> for TestName {
    fn as_ref(&self) -> &Path {
        Path::new(OsStr::new(&self.0))
    }
}

impl Display for TestName {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Debug for TestName {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

// A test task that will need to be repeated for each commit.
// TODO: this struct is too complex for the plain-old-data (pub fields)
// approach, it should be constructed with a builder.
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct Test {
    pub name: TestName,
    // Hash of the configuration that created this Test.
    pub config_hash: ConfigHash,
    pub program: OsString,
    pub args: Vec<OsString>,
    // Counts of the resource tokens this test needs a resource-token before it
    // can begin.
    pub needs_resources: HashMap<ResourceKey, usize>,
    pub shutdown_grace_period: Duration,
    pub cache_policy: CachePolicy,
    // This tests shoudln't start until these other tests have finished.
    // Manager setup will fail if there are cycles in this graph or named tests
    // do not exist.
    pub depends_on: Vec<TestName>,
}

impl Test {
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        // We want the test process to be its process group leader for two reasons:
        // - We don't want it to get SIGINTed when the user shuts down limmat,
        //   in that case we want our graceful and bugless shutdown procedure to
        //   SIGTERM it, so it can use its own graceful and bugless shutdown
        //   procedure just like it does when jobs get cancelled as normal.
        // - We wanna be able to killpg it, lol, because that seems to be the
        //   most convenient way to shut down scripts...? I dunno, man, not sure
        //   if that's a good idea.
        cmd.process_group(0);
        // Ensure we don't pass random nonsense to the test command and create
        // confusing behaviour. This is kinda annoying because IIUC this gives
        // you a fd that is immediately closed, which is likely to be different
        // from the environment where you're testing your scripts (i.e. a shell
        // prompt). You'd think just using Stdio::piped() would give a stdin
        // that is open but has nothing on it, but that isn't th behavoiur I've
        // observed, I'm not too sure why but don't wanna keep debugging this
        // forever.
        cmd.stdin(Stdio::null());
        cmd
    }

    pub fn needs_worktree(&self) -> bool {
        self.needs_resources
            .get(&ResourceKey::Worktree)
            .unwrap_or(&0)
            != &0
    }

    #[cfg(test)]
    pub fn arbitrary() -> Self {
        Test {
            name: TestName::new("my_test"),
            program: OsString::from("bash"),
            args: vec!["yer".into()],
            needs_resources: [].into(),
            shutdown_grace_period: Duration::from_secs(5),
            cache_policy: CachePolicy::ByCommit,
            config_hash: "123".to_string(),
            depends_on: vec![],
        }
    }
}

impl Display for Test {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<test: {:?}>", self.name)
    }
}

impl Debug for Test {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self)
    }
}

// This implementation is only valid for Tests among those registered for a single Manager.
// Hack: I couldn't be bothered to figure out how to make Dag work on Arc<Test>
// as well as test. Soooooo I juts define the trait for Arc<Test>... seems fine so far lmao.
impl GraphNode for Arc<Test> {
    type NodeId = TestName;

    fn id(&self) -> impl Borrow<TestName> {
        &self.name
    }

    fn child_ids(&self) -> Vec<impl Borrow<TestName>> {
        self.depends_on.iter().collect()
    }
}

pub type TestDag = Dag<Arc<Test>>;

type JobEnv = Vec<(String, String)>;

// Common elements that should be in a job environment with the given conditions.
pub fn base_job_env(repo_origin: &Path) -> JobEnv {
    vec![(
        "LIMMAT_ORIGIN".into(),
        repo_origin.to_string_lossy().into_owned(),
    )]
}

// Manages a bunch of worker threads that run tests for the current set of revisions.
pub struct Manager<W: Worktree> {
    // We hardly need this field, it should be quite easy to remove it.
    repo: Arc<W>,
    // Oops, be extremely careful about mutating this. set_revisions has some
    // pretty strong implicit assumptions about this field.
    job_cts: Mutex<HashMap<TestCaseId, CancellationToken>>,
    job_counter: JobCounter,
    notif_tx: broadcast::Sender<Arc<Notification>>,
    tests: TestDag,
    // Pools contains sets of intangible arbitrary "resources" that can be used to throttle test
    // jobs, and also tracks access to reused worktrees. The indices of the token-type resources
    // will be referenced by Test::needs_resource_idx values.
    resource_pools: Arc<Pools>,
    result_db: Arc<Database>,
    job_env: Arc<Vec<(String, String)>>,
}

// We need to specify 'static here. Just because we have an Arc over the
// repo that doesn't mean it automatically satisfies 'static:
// https://users.rust-lang.org/t/why-is-t-static-constrained-when-using-arc-t-and-thread-spawn/26262/2
// It would be much more convenient to just specify some or all these
// trait bounds as subtraits of Worktree. But I dunno, that feels Wrong.
impl<W: Worktree + Sync + Send + 'static> Manager<W> {
    pub fn new(
        // This needs to be an Arc because we hold onto a reference to it for a
        // while, and create temporary worktrees from it in the background.
        repo: Arc<W>,
        // This is mandatory instead of defaulting to the user's main database,
        // because we want it to be hard to accidentally refer to global
        // resources like that.
        result_db: Arc<Database>,
        resource_pools: Arc<Pools>,
        tests: TestDag,
    ) -> Self {
        // TODO: If this capacity gets exhausted, data gets lost and we get an error which this code
        // probably doesn't handle very gracefully. We should instead just block the sender.
        let (result_tx, _) = broadcast::channel(4096);
        Self {
            job_env: Arc::new(base_job_env(repo.path())),
            repo,
            notif_tx: result_tx,
            job_cts: Mutex::new(HashMap::new()),
            job_counter: JobCounter::new(),
            tests,
            resource_pools,
            result_db,
        }
    }

    fn spawn_job(&self, job: TestJob) {
        job.notifier.notify(&TestStatus::Enqueued);

        let pools = self.resource_pools.clone();
        let origin_worktree = self.repo.clone();
        let db = self.result_db.clone();
        tokio::spawn(async move {
            let _ = job.run(db, &pools, origin_worktree.path()).await;
        });
    }

    // Interrupt any revisions that are not in revs, start testing all revisions in revs that are
    // not already tested or being tested.
    // It doesn't make sense to call this function if you don't have a receiver
    // from already having called [[results]].
    pub async fn set_revisions<I, R>(&self, revs: I) -> anyhow::Result<()>
    where
        I: IntoIterator<Item = R>,
        R: Into<CommitHash> + Debug,
    {
        // Build the set test cases we need to kick off.
        // TODO: this is kinda silly, TestCase::new looks up the tree from the
        // hash for every indiviual test, we only need to do that at most once
        // per revision. But doing this efficiently makes for code that's
        // probably more complex than necessary.
        let commits = try_join_all(revs.into_iter().map(|rev| {
            let commit_hash = rev.into();
            self.repo
                .rev_parse(commit_hash.clone())
                .map(move |result| result?.ok_or(anyhow!("no such revision {commit_hash:?}")))
        }))
        .await?;

        self.set_commits(commits)
    }

    // Inner non-async helper for set_revisions.
    pub fn set_commits(&self, commits: impl IntoIterator<Item = Commit>) -> anyhow::Result<()> {
        let mut job_cts = self.job_cts.lock();

        let test_cases: HashMap<TestCaseId, TestCase> = commits
            .into_iter()
            .cartesian_product(self.tests.nodes())
            .map(|(commit, test)| {
                let tc = TestCase::new(commit, test.clone());
                (tc.id(), tc)
            })
            .collect();

        // Cancel jobs for test cases that we don't care about any more.
        // https://github.com/rust-lang/rust/issues/59618 would make this more convenient.
        *job_cts = job_cts
            .drain()
            .filter(|(id, cancellation_token)| {
                if !test_cases.contains_key(id) {
                    cancellation_token.cancel();
                    return false;
                }
                true
            })
            .collect::<HashMap<_, _>>();

        // Don't start new jobs for test cases that are already running
        let test_cases = test_cases.into_iter().filter_map(|(tc_id, tc)| {
            if job_cts.contains_key(&tc_id) {
                None
            } else {
                Some(tc)
            }
        });

        // Build the jobs. We do this bottom-up so that depending jobs can refer
        // to the notifier of the jobs they depend on (which we can therefore
        // trust has been constructed already).
        let test_cases = Dag::new(test_cases).expect("failed to build test case DAG");
        // Note we don't actually need the Dag structure for the jobs, and since
        // we don't have a GraphNode implementation for TestJob, we just collect
        // them into a HashMap instead.
        let jobs = test_cases.bottom_up().try_fold(
            HashMap::new(),
            |mut jobs, test_case| -> anyhow::Result<HashMap<TestCaseId, TestJob>> {
                let wait_for = test_case
                    .child_ids() // This gives the TestCaseIds of dependency jobs.
                    .iter()
                    .map(|tc_id| {
                        let dep_job = &jobs[tc_id.borrow()];
                        (dep_job.test_name().clone(), dep_job.subscribe_completion())
                    })
                    .collect();
                let job = TestJobBuilder::new(
                    CancellationToken::new(),
                    // TODO: it would be nice if we had an into_ variant of
                    // the bottom_up so we didn't need this clone.
                    test_case.clone(),
                    self.job_env.clone(),
                    wait_for,
                )
                .with_token(self.job_counter.get())
                .with_global_notif(self.notif_tx.clone())
                .build();
                jobs.insert(test_case.id(), job);
                Ok(jobs)
            },
        )?;

        for (tc_id, job) in jobs.into_iter() {
            job_cts.insert(tc_id.clone(), job.ct.clone());
            self.spawn_job(job);
        }
        Ok(())
    }

    pub async fn cancel_running(&self) -> anyhow::Result<()> {
        self.set_revisions::<_, CommitHash>([]).await
    }

    // Streams results back. Note you need to call this _before_ you generate the results you want
    // to receive.
    //
    // I think the "proper" solution for this is to return a Stream. But I don't understand it.
    pub fn results(&self) -> broadcast::Receiver<Arc<Notification>> {
        self.notif_tx.subscribe()
    }

    // Completes once there are no pending jobs or results.
    pub async fn settled(&self) {
        self.job_counter.zero().await;
    }

    pub fn into_resource_pools(self) -> Arc<Pools> {
        self.resource_pools
    }
}

struct TestStatusNotifier {
    test_case: TestCase,
    // Used to feed into the overall notification channel for observers to keep
    // track of what the whole Manager is doing.
    global_tx: Option<broadcast::Sender<Arc<Notification>>>,
    // Used to notify specifically about completion of this job. Only one mesage
    // should be sent on this channel. This is done via a separate channel so
    // that you can get notified about one job without having to wake up for a
    // bunch of other unrelated events.
    // This was originally written using a `watch` channel which I saw billed
    // online as the best way to broadcast a single value. But that's not at all
    // what it's actually designed for and using it that way makes for
    // extremely weird code.
    completion_tx: broadcast::Sender<TestOutcome>,
}

impl TestStatusNotifier {
    fn new(test_case: TestCase, global_tx: Option<broadcast::Sender<Arc<Notification>>>) -> Self {
        let completion_tx = broadcast::Sender::new(1);
        Self {
            test_case,
            global_tx,
            completion_tx,
        }
    }

    // Get notified when the job on the other end of this notifier is complete.
    fn subscribe_completion(&self) -> broadcast::Receiver<TestOutcome> {
        self.completion_tx.subscribe()
    }

    // Report a general update to the status of the test job.
    pub fn notify(&self, status: &TestStatus) {
        debug!("{:?}: {}", self.test_case, status);
        // Inner failure means nobody is listening. This is expected when running unit tests.
        let notif = Arc::new(Notification {
            test_case: self.test_case.clone(),
            status: status.clone(),
        });
        if let Some(tx) = &self.global_tx {
            let _ = tx.send(notif);
        }
    }

    // Report the final status of a test job. Will be observed by anyone who has
    // already called subscribe_completion. The nature of the channel we're
    // using here means that we can only ever reliably send one message. If we
    // got that wrong the results would be confusing to debug, so that's why
    // sending the message consumes the JobDebNotifier.
    fn notify_completion(self, outcome: TestOutcome) {
        self.notify(&TestStatus::Finished(outcome.clone()));
        // Inner failure means nobody is listening. This is fine and normal.
        let _ = self.completion_tx.send(outcome);
    }
}

// This is a horrible attempt to implement Manager::settled. There is no Condvar in tokio or
// futures-rs, so we have this weird condvar-like construction using a Tokio watch channel.
struct JobCounter {
    w: watch::Sender<usize>,
}

impl JobCounter {
    pub fn new() -> Self {
        Self {
            w: watch::Sender::new(0),
        }
    }

    // Increment the counter. It is decremented when the token is dropped.
    pub fn get(&self) -> JobToken {
        // Hack? We only report that we "modified" the value if it changed its
        // zeroness, since that's the only thing that waiters care about.
        self.w.send_if_modified(|count| {
            let was_zero = *count == 0;
            *count += 1;
            was_zero
        });
        JobToken { w: self.w.clone() }
    }

    // Block until the counter is zero. If it's already zero, return immediately. This might miss
    // transient zeroness but is guaranteed to return eventually if the counter stays zero for some
    // finite amount of time.
    pub async fn zero(&self) {
        let mut rx = {
            if *self.w.borrow() == 0 {
                return;
            }
            // Note there's a race here, we've already dropped the Ref from self.w.borrow() so the
            // counter could change. This doesn't matter because wait_for checks if the value is
            // already zero before blocking, so missed updates are harmless.
            self.w.subscribe()
        };
        rx.wait_for(|count| *count == 0)
            .await
            .expect("sender dropped in job counter");
    }
}

struct JobToken {
    w: watch::Sender<usize>,
}

impl Drop for JobToken {
    fn drop(&mut self) {
        self.w.send_if_modified(|count| {
            *count -= 1;
            *count == 0
        });
    }
}

// A drop guard which attempts to ensure child processes can be cleaned up quite
// brutally, by killing the entire process group with the PID of the child.
// This is a bad idea if you haven't made some effort to ensure that the child's
// process group ID is its PID. I initially tried to ensure that with Rust
// jiggery pokery but it produced just godawful nonsense verbosity so... just be
// careful yeah?
#[derive(Debug)]
struct ChildDropGuard(Child);

impl Drop for ChildDropGuard {
    fn drop(&mut self) {
        let pid = match self.0.id() {
            None => return, // Must already have shut down.
            Some(p) => Pid::from_raw(p.try_into().unwrap()),
        };
        killpg(pid, Signal::SIGKILL).or_log_error("SIGKILLing child process group");
    }
}

pub struct TestJobBuilder {
    ct: CancellationToken,
    test_case: TestCase,
    token: Option<JobToken>,
    env: Arc<Vec<(String, String)>>,
    wait_for: Vec<(TestName, broadcast::Receiver<TestOutcome>)>,
    global_tx: Option<broadcast::Sender<Arc<Notification>>>,
}

impl TestJobBuilder {
    pub fn new(
        ct: CancellationToken,
        test_case: TestCase,
        env: Arc<JobEnv>,
        // Job shouldn't start until all of these channels produce a result. If any
        // is unsuccessful it should abort.
        wait_for: Vec<(TestName, broadcast::Receiver<TestOutcome>)>,
    ) -> Self {
        Self {
            ct,
            test_case,
            env,
            wait_for,
            token: None,
            global_tx: None,
        }
    }

    // Have this job also report when it's done back to this weird token counter
    // mechanism that probably shouldn't exist.
    fn with_token(mut self, token: JobToken) -> Self {
        self.token = Some(token);
        self
    }

    // Have this job also report notifications about its status to this channel.
    pub fn with_global_notif(mut self, tx: broadcast::Sender<Arc<Notification>>) -> Self {
        self.global_tx = Some(tx);
        self
    }

    pub fn build(self) -> TestJob {
        TestJob {
            ct: self.ct,
            test_case: self.test_case.clone(),
            _token: self.token,
            base_env: self.env,
            wait_for: self.wait_for,
            notifier: TestStatusNotifier::new(self.test_case, self.global_tx),
        }
    }
}

// This is not really a proper type, it doesn't really mean anything except as an implementation
// detail of its user. I tried to get rid of it but then you run into issues with getting references
// to individual fields while a mutable reference exists to the overall struct. I think this is
// basically one an instance of "view structs" described in
// https://smallcultfollowing.com/babysteps/blog/2024/06/02/the-borrow-checker-within/
pub struct TestJob {
    ct: CancellationToken,
    test_case: TestCase,
    _token: Option<JobToken>,
    // Just the parts of the environment that are shared with other jobs.
    base_env: Arc<Vec<(String, String)>>,
    // Job shouldn't start until all of these channels produce a result. If any
    // is unsuccessful it should abort.
    wait_for: Vec<(TestName, broadcast::Receiver<TestOutcome>)>,
    notifier: TestStatusNotifier,
}

pub type DepDatabaseEntries = HashMap<TestName, Arc<DatabaseEntry>>;

impl<'a> TestJob {
    pub fn subscribe_completion(&self) -> broadcast::Receiver<TestOutcome> {
        self.notifier.subscribe_completion()
    }

    // This is the normal entry point to run a job. It gets the necessary
    // resources from the pools and runs the job. It takes care of notifying
    // anyone who needs to know about the result (i.e. depending jobs, and the
    // result database), and also of checking for pre-existing results in the
    // database and storing the new result if there is one.
    // TODO: It's a little awkward that this needs to return the TestOutcome. A
    // healthy user shouldn't really need to care. But, at the moment the
    // implementation of the "test" command is kinda sketchy and runs dependency
    // jobs via different logic than the "main" job.
    pub async fn run(
        mut self,
        database: Arc<Database>,
        pools: &Pools,
        origin_worktree_path: &Path,
    ) -> TestOutcome {
        let outcome = self.do_run(database, pools, origin_worktree_path).await;
        self.notifier.notify_completion(outcome.clone());
        outcome
    }

    async fn do_run(
        &mut self,
        database: Arc<Database>,
        pools: &Pools,
        origin_worktree_path: &Path,
    ) -> TestOutcome {
        // Wait for dependencies do be done, bail early if they do anything
        // but terminate successfully.
        let dep_db_entries = self
            .await_dep_success()
            .await
            .map_err(|test_name| anyhow!("dependency job {test_name} failed"))?;

        let output = match database
            .lookup(&self.test_case)
            .await
            .context("database lookup")?
        {
            LookupResult::FoundResult(db_entry) => {
                return Ok(Arc::new(db_entry));
            }
            LookupResult::YouRunIt(output) => output,
        };

        select! {
            // This "biased" is here because otherwise when we cancel a bunch of jobs all at once,
            // and some of those jobs are blocking on resources held by others,
            // we want the former jobs to observe their own cancellation before
            // they see the resources get freed up by the latter. I don't think
            // this totally eliminates that case, which probably means tests
            // will be flaky. Not sure what to do about that.
            biased;

            _ = self.ct.cancelled() => Err(TestInconclusive::Canceled),
            resources = pools.get(self.test_case.test.needs_resources.clone()) =>  {
                self.notifier.notify(&TestStatus::Started);
                if let Some(worktrees) = resources.resources(&ResourceKey::Worktree) {
                    // We "own" this worktree.
                    let worktree = worktrees[0].as_worktree();
                    worktree.checkout(&self.test_case.commit_hash).await.context("failed to check out revision")?;
                    self.execute_child(worktree.path(), &resources, output, dep_db_entries).await
                } else {
                    // We don't "own" the "main" worktree so the job shouldn't mess with it.
                    self.execute_child(origin_worktree_path, &resources, output, dep_db_entries).await
                }
            }
        }
    }

    // Blocks until all dependency jobs have succeeded and returns all the
    // database entries containing their results, or returns an error reporting
    // the name of the job that terminated without success.
    async fn await_dep_success(&mut self) -> Result<DepDatabaseEntries, TestName> {
        // This is another thing where the tokio::sync::watch API is a bit
        // weird, there's no way to wait for a message without passing a
        // predicate, so we have to pass a dummy one.
        let wait_for: Vec<_> = self
            .wait_for
            .iter_mut()
            .map(|(name, rx)| rx.recv().map(|status| (name, status)))
            .collect();
        let mut wait_for: Vec<_> = wait_for.into_iter().map(Box::pin).collect();
        let mut ret = HashMap::new();
        while !wait_for.is_empty() {
            let ((test_name, outcome), _idx, remaining) = select_all(wait_for).await;
            wait_for = remaining;
            // We are squashing lots of different types of failures and aborts
            // (including the "impossible" case that the sender has been dropped
            // and the rx.wait_for call failed) here, we trust that the other
            // side of the notifier has reported any issues appropriately.
            if let Ok(Ok(db_entry)) = &outcome {
                if db_entry.exit_code() == 0 {
                    debug!(
                        "{:?}: Dependency {:?} succeeded",
                        self.test_case.test.name, test_name
                    );
                    ret.insert(test_name.clone(), db_entry.clone());
                    continue;
                }
            }
            info!(
                "Dependency {:?} of {:?} failed: {:?}",
                test_name, self.test_case.test.name, outcome
            );
            return Err(test_name.clone());
        }
        debug!("{:?}: Dependencies succeeded", self.test_case);
        Ok(ret)
    }

    fn set_env(
        &self,
        cmd: &mut Command,
        resources: &Resources<'a>,
        artifacts_dir: &Path,
        dep_db_entries: &DepDatabaseEntries,
    ) {
        cmd.env("LIMMAT_COMMIT", &self.test_case.commit_hash);
        cmd.env("LIMMAT_ARTIFACTS", artifacts_dir);
        for (k, v) in self.base_env.iter() {
            cmd.env(k, v);
        }
        // Set up env vars to communicate token values.
        for (resource_name, tokens) in resources.tokens() {
            if tokens.len() == 1 {
                cmd.env(format!("LIMMAT_RESOURCE_{}", resource_name), &tokens[0]);
            }
            for (i, token) in tokens.iter().enumerate() {
                cmd.env(format!("LIMMAT_RESOURCE_{}_{}", resource_name, i), token);
            }
        }
        for (test_name, db_entry) in dep_db_entries {
            cmd.env(
                format!("LIMMAT_ARTIFACTS_{}", test_name),
                db_entry.artifacts_dir(),
            );
        }
    }

    // The core part of the job - runs the actual process and returns its result.
    async fn execute_child(
        &mut self,
        current_dir: &Path,
        resources: &Resources<'a>,
        mut output: DatabaseOutput,
        dep_db_entries: DepDatabaseEntries,
    ) -> TestOutcome {
        info!("Starting {:?}", self.test_case);

        let mut cmd = self.test_case.test.command();
        cmd.current_dir(current_dir)
            .stdout(output.stdout().context("no stdout handle available")?)
            .stderr(output.stderr().context("no stdout handle available")?);
        self.set_env(&mut cmd, resources, output.artifacts_dir(), &dep_db_entries);
        // It would be really confusing and annoying if we exited this function
        // without ensuring the child is dead. So we wrap it in this sketchy
        // drop guard thing.
        let mut child = ChildDropGuard(cmd.spawn().context("spawning test command")?);
        // Grab the PID now if we can, since it's a pain to look it up later for
        // silly Rust reasons. If no PID is found we just carry on assuming the
        // process has already shut down.
        let pid = child
            .0
            .id()
            .map(|raw| Pid::from_raw(raw.try_into().unwrap()));
        // Await the child, or cancellation. Because the "right" branch still needs to do work on
        // the "left" future, tokio::select doesn't grant us any clarity or concision here so we
        // drop down to the raw function call.
        let child_fut = pin!(child.0.wait());
        let cancel_fut = pin!(self.ct.cancelled());
        match future::select(child_fut, cancel_fut).await {
            Either::Left((wait_result, _)) =>
            // Test completed, figure out the result. I think maybe a true Rustacean would
            // write this block as a single chain of methods? But it seems ridiculous to me.
            {
                let exit_code = wait_result.context("awaiting child")?.code_not_killed()?;
                Ok(Arc::new(
                    output.set_result(&TestResult { exit_code }).await?,
                ))
            }
            Either::Right((_, child_fut)) => {
                // Canceled. Shut down the process if necessary.
                if let Some(pid) = pid {
                    killpg(pid, Signal::SIGTERM).or_log_error("SIGTERMing child process");
                }
                // We don't care about its result but we
                // need to wait for it to shut down so that we can safely give back the
                // worktree.
                let timeout = pin!(sleep(self.test_case.test.shutdown_grace_period));
                match future::select(child_fut, timeout).await {
                    Either::Left(_) => (), // Done, child terminated
                    Either::Right((_timeout, child_fut)) => {
                        // Canceled. Shut down the process harder.
                        warn!(
                            "timeout for {:?}, SIGKILLing whole process group",
                            self.test_case.test.name
                        );
                        killpg(pid.expect("timed out, but no child PID"), Signal::SIGKILL)
                            .or_log_error("SIGKILLing child process group");
                        // To be sure to be sure, we'll also wait and make sure
                        // the child is really dead.
                        child_fut.await.expect("failed to wait on SIGKILLed child");
                    }
                }

                Err(TestInconclusive::Canceled)
            }
        }
    }

    // This is a specialised entry point for when you already have the necessary
    // resources from the pools and you need direct control over where the job
    // runs and where its output goes. This API is wack and I hate it but I
    // can't quite seem to figure out the right design in snatched moments on
    // weeknights.
    pub async fn run_with(
        mut self,
        current_dir: &Path,
        resources: &Resources<'a>,
        output: DatabaseOutput,
        dep_db_entries: DepDatabaseEntries,
    ) -> TestOutcome {
        let outcome = self
            .execute_child(current_dir, resources, output, dep_db_entries)
            .await;
        self.notifier.notify_completion(outcome.clone());
        outcome
    }

    pub fn test_name(&self) -> &TestName {
        &self.test_case.test.name
    }
}

// An identifier that uniquely identifies a TestCase among all that can exist for a given Manager.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TestCaseId(String);

impl TestCaseId {
    fn new(commit_hash: &CommitHash, test_name: &TestName) -> Self {
        Self(format!("{}:{}", commit_hash, test_name))
    }
}

#[derive(Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct TestCase {
    // Commit that will be checked out to run the test.
    pub commit_hash: CommitHash,
    // Hash that will be used to identify the test result. Might be a tree hash,
    // otherwise it matches the commit hash.
    pub cache_hash: Option<Hash>,
    pub test: Arc<Test>,
}

impl Debug for TestCase {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TestCase({:?}, {:?})",
            self.commit_hash.abbrev(),
            self.test.name
        )
    }
}

impl TestCase {
    pub fn new(commit: Commit, test: Arc<Test>) -> Self {
        Self {
            cache_hash: test.cache_policy.cache_hash(&commit),
            test,
            commit_hash: commit.hash,
        }
    }

    // Returns the hash that should be used to store the result in the result
    // database. Note that results get stored in the database even when caching
    // is disabled, so that the user can see the output..
    pub fn storage_hash(&self) -> &Hash {
        self.cache_hash.as_ref().unwrap_or(&self.commit_hash)
    }

    // TODO: this is always getting built on-demand all over the place, it
    // doesn't really need to be.
    fn id(&self) -> TestCaseId {
        // The hash_cache is redundant information here so we don't need to include it.
        TestCaseId::new(&self.commit_hash, &self.test.name)
    }
}

impl GraphNode for TestCase {
    type NodeId = TestCaseId;

    fn id(&self) -> impl Borrow<TestCaseId> {
        self.id()
    }

    fn child_ids(&self) -> Vec<impl Borrow<TestCaseId>> {
        self.test
            .depends_on
            .iter()
            .map(|test_name| TestCaseId::new(&self.commit_hash, test_name))
            .collect()
    }
}

pub type ExitCode = i32;

#[derive(Debug, Clone)]
pub enum TestStatus {
    Enqueued,
    Started,
    Finished(TestOutcome),
}

impl Display for TestStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enqueued => write!(f, "Enqueued"),
            Self::Started => write!(f, "Started"),
            Self::Finished(Err(inconclusive)) => write!(f, "{}", inconclusive),
            Self::Finished(Ok(db_entry)) => write!(f, "{}", db_entry.result()),
        }
    }
}

// Final result of an attempt to run a test.
pub type TestOutcome = Result<Arc<DatabaseEntry>, TestInconclusive>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestInconclusive {
    Canceled,
    // anyhow::Error doesn't implement Clone.
    Error(String), // This includes the test getting terminated by a signal.
}

impl Display for TestInconclusive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Canceled => write!(f, "Canceled"),
            Self::Error(msg) => write!(f, "Error while testing - {:?}", msg),
        }
    }
}

impl From<anyhow::Error> for TestInconclusive {
    fn from(err: anyhow::Error) -> TestInconclusive {
        TestInconclusive::Error(format!("{err:#}"))
    }
}

impl Error for TestInconclusive {}

// Result of a test that ran to completion.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TestResult {
    // Note this is called "exit_code" instead of "return_code" because it really
    // only gets set when the child process exits.
    pub exit_code: ExitCode,
}

impl Display for TestResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "exit code {}", self.exit_code)
    }
}

#[derive(Debug)]
pub struct Notification {
    pub test_case: TestCase,
    pub status: TestStatus,
}

#[cfg(test)]
mod tests {
    use std::{
        cmp::max,
        collections::VecDeque,
        env,
        fs::{self, remove_file, File},
        io::{self, BufRead as _},
        mem::ManuallyDrop,
        path::PathBuf,
        thread::panicking,
        time::Duration,
    };

    use anyhow::bail;
    use future::{join_all, select_all};
    use googletest::{
        description::Description,
        fail,
        matcher::MatcherResult,
        prelude::{eq, Matcher, MatcherBase},
        verify_that,
    };
    use itertools::izip;
    use tempfile::TempDir;
    use test_case::test_case;
    use tokio::{
        select,
        time::{sleep, sleep_until, Instant},
    };

    use crate::{
        git::{
            test_utils::{TempRepo, WorktreeExt},
            CommitHash, TempWorktree,
        },
        resource::Resource,
        test_utils::{path_exists, timeout_5s},
    };

    use super::*;

    // A script that can be used as the test command for a Manager, with utilities for testing the
    // manager. The script won't terminate until told to.
    struct TestScript {
        dir: TempDir,
        script: OsString, // Raw content.
        test_name: TestName,
    }

    impl TestScript {
        const PID_FILENAME_PREFIX: &'static str = "pid.";
        // Each time the script gets started it echoes a line to this file.
        const RUN_COUNT_FILENAME_PREFIX: &'static str = "runs.";
        const SIGTERMED_FILENAME_PREFIX: &'static str = "sigtermed.";
        const LOCK_FILENAME: &'static str = "lockfile";
        const BUG_DETECTED_PATH: &'static str = "bug_detected";

        // If this appears in the commit message , the test script will block
        // until SIGTERMed, otherwise it terminates immediately. When receiving
        // SIGTERM the value depends on whether you include the results of
        // exit_code_tag(). If yes, it exits with that code, otherwise it is
        // terminated directly by the signal (the latter is considered an
        // "error" by limmat).
        // We would like this to be an OsStr but you can't do that according to
        // https://stackoverflow.com/questions/49226783/is-there-any-way-to-represent-an-osstr-or-osstring-literal
        pub const BLOCK_COMMIT_MSG_TAG: &'static str = "block_this_test";

        // Generate a tag which, when put in the commit message of a commit, will result in the test
        // returning the given exit code.
        pub fn exit_code_tag(code: u32) -> OsString {
            format!("exit_code({})", code).into()
        }

        // Creates a script, this will create a temporary directory, which will
        // be destroyed on drop.
        pub fn new(test_name: TestName, use_lockfile: bool) -> Self {
            let dir = TempDir::with_prefix("test-script-").expect("couldn't make tempdir");
            // The script will touch a special file to notify us that it has
            // been started. On receiving SIGTERM it touches a nother special
            // file. Then if Terminate::Never it blocks forever.
            //
            // The "lockfile" lets us detect if the worktree gets assigned to multiple script
            // instances at once. We would ideally actually do this with flock but it turns out to
            // be a bit of a pain to use, so we just use regular if-statements. I _guess_ we can
            // trust from the PoV of a single thread that this will be consistent, i.e. it cannot
            // produce false positive failures. I am sure that it can produce false negatives, but
            // we could get false negatives here even with flock, since there is always a window
            // between the script starting and it actually taking the lock.
            //
            // A confusing note about Bash: even though we have traps in place,
            // if bash gets signalled it will appear to have been terminated by
            // that signal. I dunno how this happens, but I did check and it
            // does happen. Either there's a way to set up signal handlers so
            // that you can both have a handler _and_ be terminated, or Bash
            // does something weird like run its handler and then signal itself
            // again.
            let script = format!(
                "
                # I dunno this is a desperate attempt to debug this crap, set -x doesn't seem
                # to place nicely enough with traps and shit. But this also
                # dumps a bunch of NULs to stderr. WTF.
                debug() {{
                    echo \"$BASH_COMMAND\" | tr -d '\\000' >&2
                }}
                trap debug DEBUG

                commit_msg=\"$(git log -n1 --format=%B $LIMMAT_COMMIT)\"
                exit_code=$(echo \"$commit_msg\" | perl -n -e'/exit_code\\((\\d+)\\)/ && print $1')
                on_sigterm() {{
                    touch {sigtermed_path_prefix:?}$(git rev-parse $LIMMAT_COMMIT)
                    exit ${{exit_code:-0}}
                }}
                trap on_sigterm SIGTERM

                # Write then move, to make populated file appear atomically.
                # Note also we mustn't make the PID file visible until after we
                # have installed the  trap - see the comment on
                # StartedTestScript::siginted.
                pid_file=$(mktemp)
                echo $$ >> $pid_file
                mv $pid_file {pid_path_prefix:?}$(git rev-parse $LIMMAT_COMMIT)

                echo >> {run_count_path_prefix:?}$(git rev-parse $LIMMAT_COMMIT)

                if [ -n \"{lock_filename}\" ]; then
                    if [ -e ./{lock_filename:?} ]; then
                        echo 'Overlapping test script runs used the same worktree (detected by {test_name:?}' \
                            >> {bug_detected_path:?}
                    fi
                    trap \"rm {lock_filename:?}\" EXIT
                    touch ./{lock_filename:?}
                fi
                if [[ \"$commit_msg\" =~ {block_tag} ]]; then
                    # sleep is not a builtin so we won't handle SIGTERM while
                    # that's running. Hack suggested by ChatGPT: just spawn it
                    # then use wait, which is a builtin.
                    sleep infinity &
                    wait $!
                fi
                # Extract the exit code and pass it to exit if there is one, otherwise pass 0.
                exit ${{exit_code:-0}}
                ",
                run_count_path_prefix = dir.path().join(Self::RUN_COUNT_FILENAME_PREFIX),
                pid_path_prefix = dir.path().join(Self::PID_FILENAME_PREFIX),
                sigtermed_path_prefix = dir.path().join(Self::SIGTERMED_FILENAME_PREFIX),
                lock_filename = if use_lockfile { Self::LOCK_FILENAME } else { "" },
                bug_detected_path = dir.path().join(Self::BUG_DETECTED_PATH),
                block_tag = Self::BLOCK_COMMIT_MSG_TAG,
            );

            Self {
                dir,
                test_name,
                script: script.into(),
            }
        }

        // Pass this to Manager::new
        pub fn program(&self) -> OsString {
            "bash".into()
        }
        // Pass this to Manager::new
        pub fn args(&self) -> Vec<OsString> {
            vec!["-xc".into(), self.script.clone()]
        }

        // Path used by the running script to signal an event.
        fn signalling_path(&self, filename_prefix: &str, hash: &CommitHash) -> PathBuf {
            // Argh I dunno this is annoying.
            let mut filename = OsString::from(filename_prefix);
            filename.push(hash);
            self.dir.path().join(filename)
        }

        // If this path exists, two instances of the script used the same worktree at once.
        fn bug_detected_path(&self) -> PathBuf {
            self.dir.path().join(Self::BUG_DETECTED_PATH)
        }

        // Has the script been started so far for this test?
        pub fn was_started(&self, hash: &CommitHash) -> bool {
            self.signalling_path(Self::PID_FILENAME_PREFIX, hash)
                .exists()
        }

        // Blocks until the script is started for the given commit hash.
        pub async fn started(&self, hash: &CommitHash) -> StartedTestScript {
            let pid_path = self.signalling_path(Self::PID_FILENAME_PREFIX, hash);
            path_exists(&pid_path).await;
            let content = fs::read_to_string(pid_path).expect("couldn't read PID file");
            StartedTestScript {
                script: self,
                hash: hash.to_owned(),
                pid: Pid::from_raw(content.trim().parse().unwrap_or_else(|_| {
                    panic!("couldn't parse PID file as integer (content: {content:?})")
                })),
            }
        }

        // Number of times the script has been successfully spawned for this
        // commit, since StartedTestScript::reset_started was called for it.
        pub fn num_runs(&self, hash: &CommitHash) -> usize {
            let path = self.signalling_path(Self::RUN_COUNT_FILENAME_PREFIX, hash);
            if !path.exists() {
                return 0;
            }
            // Awkward: it's probably still possible to return 0 here, we can
            // probably observe the file whe it exists but doesn't have any content yet.
            io::BufReader::new(File::open(path).expect("couldn't open TestScript signalling file"))
                .lines()
                .count()
        }

        pub fn as_test(
            &self,
            cache_policy: CachePolicy,
            needs_worktree: bool,
            depends_on: impl IntoIterator<Item = TestName>,
        ) -> Test {
            Test {
                name: self.test_name.clone(),
                program: self.program(),
                args: self.args(),
                needs_resources: if needs_worktree {
                    [(ResourceKey::Worktree, 1)].into()
                } else {
                    [].into()
                },
                shutdown_grace_period: Duration::from_secs(5),
                cache_policy,
                config_hash: "0".to_string(),
                depends_on: depends_on.into_iter().collect(),
            }
        }
    }

    // Hack to check for stuff that is orthogonal to any particular test, so we
    // don't wanna have to it in every individual test.
    impl Drop for TestScript {
        fn drop(&mut self) {
            let path = self.bug_detected_path();
            if path.exists() {
                let content =
                    std::fs::read_to_string(path).expect("couldn't read bug-detected path");
                let msg = format!(
                    "The test script {:?} detected one or more bugs: {}",
                    self.test_name, content
                );
                if panicking() {
                    // If you panic during a panic (i.e. if this fails when the test had already
                    // failed) you get a huge splat. Just log instead.
                    error!("{}", msg);
                } else {
                    panic!("{}", msg);
                }
            }
        }
    }

    // Like a TestScript, but you can only get one once it's already startd running, so it has extra
    // operations.
    struct StartedTestScript<'a> {
        script: &'a TestScript,
        hash: CommitHash,
        pid: Pid,
    }

    impl StartedTestScript<'_> {
        // Blocks until the script has received a SIGTERM.
        // Note this is only available on StartedTestScript, because we cannot
        // reliably receive notifications of the script getting SIGTERMed unless
        // we know that the script was able to get its SIGTERM trap installed
        // before it got signalled.
        pub async fn sigtermed(&self) {
            path_exists(
                self.script
                    .signalling_path(TestScript::SIGTERMED_FILENAME_PREFIX, &self.hash),
            )
            .await;
        }

        // Send SIGUSR1 to the instance of the script. Use this if you want the
        // script to "fail with an error". This preferable to SIGKILL because
        // that will prevent the underlying script from performing its cleanup.
        pub fn sigurs1(&self) {
            killpg(self.pid, Signal::SIGUSR1).expect("couldn't SIGUSR1 test script");
        }

        // Forget "started" state so that TestScript::started can usefully be called again.
        pub fn reset_started(self) {
            remove_file(
                self.script
                    .signalling_path(TestScript::PID_FILENAME_PREFIX, &self.hash),
            )
            .expect("couldn't delete test script PID file");
        }
    }

    #[derive(MatcherBase, Debug)]
    enum TestStatusMatcher {
        Enqueued,
        Started,
        Completed(ExitCode),
        Inconclusive(TestInconclusive),
    }

    impl Matcher<&TestStatus> for TestStatusMatcher {
        fn matches(&self, actual: &TestStatus) -> MatcherResult {
            match (self, actual) {
                (Self::Enqueued, TestStatus::Enqueued) => MatcherResult::Match,
                (Self::Started, TestStatus::Started) => MatcherResult::Match,
                (Self::Completed(exit_code), TestStatus::Finished(Ok(db_entry))) => {
                    if db_entry.exit_code() == *exit_code {
                        MatcherResult::Match
                    } else {
                        MatcherResult::NoMatch
                    }
                }
                (Self::Inconclusive(want), TestStatus::Finished(Err(got))) => eq(want).matches(got),
                _ => MatcherResult::NoMatch,
            }
        }

        fn describe(&self, matcher_result: MatcherResult) -> Description {
            match matcher_result {
                // TODO: hmmm..?
                MatcherResult::Match => format!("is equal to {:?}", self).into(),
                MatcherResult::NoMatch => format!("isn't equal to {:?}", self).into(),
            }
        }
    }

    // Expect the series of notifications provided for each test case.
    // case. Also assert that the necessary precursor notifications arrive.
    // Panics if any of the input series are empty.
    async fn expect_notifs_20s(
        results: &mut broadcast::Receiver<Arc<Notification>>,
        matchers: impl IntoIterator<Item = (TestCase, VecDeque<TestStatusMatcher>)>,
    ) -> googletest::Result<()> {
        let timeout = Instant::now() + Duration::from_secs(20);
        let mut matchers: HashMap<TestCaseId, _> = matchers
            .into_iter()
            .map(|(test_case, statuses)| (test_case.id(), (test_case, statuses)))
            .collect();
        let want_total = matchers.len();
        while !matchers.is_empty() {
            let notif = select! {
                _ = sleep_until(timeout) => {
                    return fail!("timeout after 20s, {} remaining results of {}",
                        matchers.len(), want_total);
                },
                output = results.recv() => {
                    output? // TODO: Descriptive failure (result stream treminated)
                }
            };
            let (_tc, tc_matchers) = matchers
                .get_mut(&notif.test_case.id())
                .unwrap_or_else(|| panic!("got notification for unexpected test case: {notif:?}"));
            let matcher = tc_matchers.pop_front().expect("empty matcher series");
            if tc_matchers.is_empty() {
                matchers.remove(&notif.test_case.id());
            }
            // TODO: Why do I need to take references to both of these?? WTF?
            verify_that!(&notif.status, matcher)?;
        }
        Ok(())
    }

    async fn expect_no_more_results(
        results: &mut broadcast::Receiver<Arc<Notification>>,
        m: &Manager<TempRepo>,
    ) -> anyhow::Result<()> {
        select! {
            _ = sleep(Duration::from_secs(1)) => bail!("didn't settle after 1s"),
            result = results.recv() => bail!("unexpected test result received: {:?}", result),
            _ = m.settled() => Ok(())
        }
    }

    struct TestScriptFixtureBuilder {
        num_worktrees: usize,
        num_tests: usize,
        // If we end up with more awkward "configuration vectors" like these
        // then probably we should just switch this builder over to constructing
        // via config::Test::parse.
        cache_policies: Vec<CachePolicy>,
        needs_worktree: Vec<bool>,
        dependencies: Vec<(usize, usize)>,
    }

    impl TestScriptFixtureBuilder {
        pub fn num_worktrees(mut self, n: usize) -> Self {
            self.num_worktrees = n;
            self
        }

        // Ensure we are prepared to set up at least n tests.
        fn extend(mut self, n: usize) -> Self {
            while self.cache_policies.len() < n {
                self.cache_policies.push(CachePolicy::ByCommit);
            }
            while self.needs_worktree.len() < n {
                self.needs_worktree.push(true);
            }
            self
        }

        // Tests per commit.
        pub fn num_tests(mut self, n: usize) -> Self {
            self.num_tests = n;
            self.extend(n)
        }

        // cache_policies[i] will be the cache policy for the ith test.
        pub fn cache_policies(mut self, pols: impl IntoIterator<Item = CachePolicy>) -> Self {
            self.cache_policies = pols.into_iter().collect();
            let len = self.cache_policies.len();
            self.num_tests(len)
        }

        // needs_worktrees[i] will decide if the ith test requires a worktree.
        pub fn needs_worktree(mut self, needs_worktree: impl IntoIterator<Item = bool>) -> Self {
            self.needs_worktree = needs_worktree.into_iter().collect();
            let len = self.needs_worktree.len();
            self.num_tests(len)
        }

        // Declare pairs of text indexes where the first depends on the second.
        pub fn dependencies(mut self, deps: impl IntoIterator<Item = (usize, usize)>) -> Self {
            self.dependencies = deps.into_iter().collect();
            let max_idx = self
                .dependencies
                .iter()
                .map(|&(x, y)| max(x, y))
                .max()
                .unwrap_or(0);
            self.extend(max_idx + 1)
        }
    }

    async fn nonempty_temp_repo() -> Arc<TempRepo> {
        let repo = Arc::new(TempRepo::new().await.unwrap());
        // TODO: We need to have a commit in the repo otherwise manager
        // setup will fail. This is a bug.
        repo.commit("hello,")
            .await
            .expect("couldn't create base commit");
        repo
    }

    async fn worktree_resources(origin: &TempRepo, n: usize) -> Vec<Resource> {
        try_join_all((0..n).map(|_| async {
            let t = TempDir::with_prefix("worktree").context("creating tempdir")?;
            Ok::<_, anyhow::Error>(Resource::Worktree(
                TempWorktree::new(&CancellationToken::new(), origin, t)
                    .await
                    .context("creating worktree")?,
            ))
        }))
        .await
        .unwrap()
    }

    struct TestScriptFixture {
        db_dir: ManuallyDrop<TempDir>,
        repo: Arc<TempRepo>,
        scripts: Vec<TestScript>,
        manager: Manager<TempRepo>,
    }

    impl TestScriptFixtureBuilder {
        pub async fn build(&self) -> TestScriptFixture {
            let repo = nonempty_temp_repo().await;
            let scripts: Vec<TestScript> = (0..self.num_tests)
                // Here we pass needs_worktree[i] to configure whether the script should
                // try to detect sharing a worktree with another test run. If it
                // doesn't a worktree then that sharing is harmless and
                // expected.
                .map(|i| {
                    TestScript::new(TestName::new(format!("test_{i}")), self.needs_worktree[i])
                })
                .collect();
            let db_dir =
                TempDir::with_prefix("result-db-").expect("couldn't make temp dir for result DB");
            let tests = izip!(
                0..self.num_tests,
                &scripts,
                &self.cache_policies,
                &self.needs_worktree
            )
            .map(|(i, script, &cache_policy, &needs_worktree)| {
                let dep_names = self
                    .dependencies
                    .iter()
                    .filter(|(from_idx, _)| *from_idx == i)
                    .map(|(_, to_idx)| TestName::new(format!("test_{to_idx}")));
                script.as_test(cache_policy, needs_worktree, dep_names)
            });
            let manager = Manager::new(
                repo.clone(),
                Arc::new(
                    Database::create_or_open(db_dir.path()).expect("couldn't setup result DB"),
                ),
                Arc::new(Pools::new([(
                    ResourceKey::Worktree,
                    worktree_resources(&repo, self.num_worktrees).await,
                )])),
                Dag::new(tests.map(Arc::new)).expect("couldn't build test DAG"),
            );
            TestScriptFixture {
                manager,
                scripts,
                repo,
                db_dir: ManuallyDrop::new(db_dir),
            }
        }
    }

    impl TestScriptFixture {
        pub fn builder() -> TestScriptFixtureBuilder {
            TestScriptFixtureBuilder {
                num_worktrees: 2,
                num_tests: 2,
                cache_policies: vec![CachePolicy::ByCommit; 2],
                needs_worktree: vec![true; 2],
                dependencies: vec![],
            }
        }

        // Convenience helper to construct a TestCase referring to this fixture's configuration.
        // yes this function is O(test_idx). you got a porblem with that?? is that a porblem?
        fn test_case(&self, commit: impl Borrow<Commit>, test_idx: usize) -> TestCase {
            TestCase::new(
                commit.borrow().to_owned(),
                self.manager
                    .tests
                    .nodes()
                    .nth(test_idx)
                    .expect("bad test idx")
                    .clone(),
            )
        }
    }

    impl Drop for TestScriptFixture {
        fn drop(&mut self) {
            // SAFETY: The field is never accessed again.
            let db_dir = unsafe { ManuallyDrop::take(&mut self.db_dir) };
            if env::var("LIMMAT_TESTS_LEAK_RESULT_DB").unwrap_or("0".to_owned()) != "0" {
                let db_dir_path = db_dir.into_path(); // Stops it from being deleted.
                info!("Leaking database directory {:?}", db_dir_path);
            }
        }
    }

    #[test_log::test(tokio::test)]
    async fn should_run_single() {
        let f = TestScriptFixture::builder().num_tests(1).build().await;
        let mut results = f.manager.results();
        let commit = f
            .repo
            .commit("hello,")
            .await
            .expect("couldn't create test commit");
        f.manager.set_revisions(vec![commit.clone()]).await.unwrap();
        // We should get a singular result because we only fed in one revision.
        expect_notifs_20s(
            &mut results,
            [(
                f.test_case(&commit, 0),
                vec![
                    TestStatusMatcher::Enqueued,
                    TestStatusMatcher::Started,
                    TestStatusMatcher::Completed(0),
                ]
                .into(),
            )],
        )
        .await
        .expect("bad test result");
        expect_no_more_results(&mut results, &f.manager)
            .await
            .unwrap()
    }

    #[test_log::test(tokio::test)]
    async fn should_cancel_running() {
        let f = TestScriptFixture::builder().num_tests(2).build().await;
        // First commit's test will block forever.
        let commit1 = f
            .repo
            .commit(TestScript::BLOCK_COMMIT_MSG_TAG)
            .await
            .expect("couldn't create test commit");
        let mut results = f.manager.results();
        f.manager
            .set_revisions(vec![commit1.clone()])
            .await
            .unwrap();
        let started_commit1 =
            timeout_5s(join_all(f.scripts.iter().map(|s| s.started(&commit1.hash))))
                .await
                .expect("not all scripts run for hash1");
        // Second commit's test will terminate quickly.
        let commit2 = f
            .repo
            .commit("hello,")
            .await
            .expect("couldn't create test commit");
        f.manager
            .set_revisions(vec![commit2.clone()])
            .await
            .unwrap();
        timeout_5s(f.scripts[0].started(&commit2.hash))
            .await
            .expect("f.scripts[0] did not run for hash2");
        timeout_5s(join_all(
            started_commit1
                .iter()
                .map(|s: &StartedTestScript| s.sigtermed()),
        ))
        .await
        .expect("hash1 tests did not all get siginted");
        expect_notifs_20s(
            &mut results,
            // awu weh, weh mah
            [
                (
                    f.test_case(&commit1, 0),
                    vec![
                        TestStatusMatcher::Enqueued,
                        TestStatusMatcher::Started,
                        TestStatusMatcher::Inconclusive(TestInconclusive::Canceled),
                    ]
                    .into(),
                ),
                (
                    f.test_case(&commit1, 1),
                    vec![
                        TestStatusMatcher::Enqueued,
                        TestStatusMatcher::Started,
                        TestStatusMatcher::Inconclusive(TestInconclusive::Canceled),
                    ]
                    .into(),
                ),
                // This isn't what we're testing here but we need to assert that it comes in so we can
                // check below that nothing else comes in.
                (
                    f.test_case(&commit2, 0),
                    vec![
                        TestStatusMatcher::Enqueued,
                        TestStatusMatcher::Started,
                        TestStatusMatcher::Completed(0),
                    ]
                    .into(),
                ),
                (
                    f.test_case(&commit2, 1),
                    vec![
                        TestStatusMatcher::Enqueued,
                        TestStatusMatcher::Started,
                        TestStatusMatcher::Completed(0),
                    ]
                    .into(),
                ),
            ],
        )
        .await
        .unwrap();
        expect_no_more_results(&mut results, &f.manager)
            .await
            .unwrap()
    }

    // This is not actually testing functionality, this is a meta-test, yikes this is
    // over-engineered.
    #[test_log::test(tokio::test)]
    async fn should_not_settle() {
        let f = TestScriptFixture::builder().num_tests(1).build().await;
        // First commit's test will block forever.
        let commit = f
            .repo
            .commit(TestScript::BLOCK_COMMIT_MSG_TAG)
            .await
            .expect("couldn't create test commit");
        f.manager.set_revisions([commit.clone()]).await.unwrap();
        timeout_5s(f.scripts[0].started(&commit.hash))
            .await
            .expect("script did not start");
        select! {
            _ = sleep(Duration::from_secs(1)) => (),
            _ = f.manager.settled() => panic!("manager settled unexpectedly"),
        }
    }

    #[test_log::test(tokio::test)]
    async fn should_cache_results() {
        let f = TestScriptFixture::builder()
            .cache_policies([
                CachePolicy::NoCaching,
                CachePolicy::ByCommit,
                CachePolicy::ByTree,
            ])
            .build()
            .await;
        f.repo
            .commit("yarp")
            .await
            .expect("couldn't create test commit");

        // Set up two commits to test. Note this is a bit of an odd test case.
        // The real world case we are thinking of here is probably swiching
        // between two branches with a common base rather than two ranges with
        // an ancestry relation. But to do that we'd need more helpers in our
        // Git library. So we just take advantage of our knowledge that this
        // weirdness of the test case is irrelevant given how the implementation
        // works (it doesn't know about the structure of the history).
        let orig_commit = f
            .repo
            .commit("yarp")
            .await
            .expect("couldn't create test commit");
        // This one has a different commit hash but the same tree.
        let same_tree = f
            .repo
            .commit("darp")
            .await
            .expect("couldn't create test commit");

        // Test the first commit and wait for it to be complete.
        f.manager
            .set_revisions(vec![orig_commit.clone()])
            .await
            .unwrap();
        f.manager.settled().await;
        // Sanity check that the scripts actually got run.
        assert_eq!(f.scripts[0].num_runs(&orig_commit.hash), 1);
        assert_eq!(f.scripts[1].num_runs(&orig_commit.hash), 1);
        assert_eq!(f.scripts[2].num_runs(&orig_commit.hash), 1);

        f.manager
            .set_revisions(vec![same_tree.clone()])
            .await
            .unwrap();
        f.manager.settled().await;
        // Without caching the script should get run every time.
        assert_eq!(f.scripts[0].num_runs(&same_tree.hash), 1);
        // Commit has has changed so new commit should get retested for ByCommit.
        assert_eq!(f.scripts[1].num_runs(&same_tree.hash), 1);
        // But not for ByTree
        assert_eq!(f.scripts[2].num_runs(&same_tree.hash), 0);

        f.manager
            .set_revisions(vec![orig_commit.clone()])
            .await
            .unwrap();
        f.manager.settled().await;
        assert_eq!(f.scripts[0].num_runs(&orig_commit.hash), 2);
        assert_eq!(f.scripts[1].num_runs(&orig_commit.hash), 1);
        assert_eq!(f.scripts[2].num_runs(&orig_commit.hash), 1);
    }

    #[test_case(1, 1 ; "single worktree, one test")]
    #[test_case(4, 1 ; "multiple worktrees, one test")]
    #[test_case(4, 4 ; "multiple worktrees, multiple tests")]
    #[test_log::test(tokio::test)]
    async fn should_handle_many(num_worktrees: usize, num_tests: usize) {
        let f = TestScriptFixture::builder()
            .num_tests(num_tests)
            .num_worktrees(num_worktrees)
            .build()
            .await;
        let mut commits = Vec::new();
        let mut want_results = Vec::new();
        for i in 0..50 {
            let commit = f
                .repo
                .commit(TestScript::exit_code_tag(i as u32))
                .await
                .expect("couldn't create test commit");
            for j in 0..num_tests {
                want_results.push((
                    f.test_case(&commit, j),
                    vec![
                        TestStatusMatcher::Enqueued,
                        TestStatusMatcher::Started,
                        TestStatusMatcher::Completed(i),
                    ]
                    .into(),
                ));
            }
            commits.push(commit);
        }
        let mut results = f.manager.results();
        f.manager.set_revisions(commits.clone()).await.unwrap();
        expect_notifs_20s(&mut results, want_results)
            .await
            .expect("bad results");
    }

    #[test_log::test(tokio::test)]
    async fn should_respect_resource_limits() {
        let repo = Arc::new(TempRepo::new().await.unwrap());
        let mut hashes = Vec::new();
        for _ in 0..10 {
            hashes.push(
                repo.commit(TestScript::BLOCK_COMMIT_MSG_TAG)
                    .await
                    .expect("couldn't create test commit"),
            );
        }
        let script = TestScript::new(TestName::new("my_test"), true);
        // We only have 2 tokens
        let resource_tokens = HashMap::from([(
            ResourceKey::UserToken("foo".into()),
            vec![
                Resource::UserToken("foo1".into()),
                Resource::UserToken("foo2".into()),
            ],
        )]);
        // And a test that requires one of those tokens.
        let tests = [Test {
            name: TestName::new("my_test"),
            program: script.program(),
            args: script.args(),
            needs_resources: HashMap::from([
                (ResourceKey::Worktree, 1),
                (ResourceKey::UserToken("foo".into()), 1),
            ]),
            shutdown_grace_period: Duration::from_secs(5),
            cache_policy: CachePolicy::ByCommit,
            config_hash: "0".to_string(),
            depends_on: vec![],
        }];
        let db_dir = TempDir::new().expect("couldn't make temp dir for result DB");
        let m = Manager::new(
            repo.clone(),
            Arc::new(Database::create_or_open(db_dir.path()).expect("couldn't setup result DB")),
            Arc::new(Pools::new(
                [(ResourceKey::Worktree, worktree_resources(&repo, 4).await)]
                    .into_iter()
                    .chain(resource_tokens.into_iter()),
            )),
            Dag::new(tests.map(Arc::new)).expect("couldn't build test DAG"),
        );
        m.set_revisions(hashes.clone()).await.unwrap();

        let mut start_futs = hashes
            .iter()
            .map(|c| Box::pin(script.started(&c.hash)))
            .collect();
        for _ in 0..2 {
            let (_started, _index, remaining) = timeout_5s(select_all(start_futs))
                .await
                .expect("didn't start first two jobs");
            start_futs = remaining;
        }

        // Ugh, dunno how to do this except just wait for 1s...
        select! {
            _ = sleep(Duration::from_secs(1)) => (), // OK, nothing else ran.
            _ = select_all(start_futs) => panic!("extra jobs started, resource limits not respected"),
        }
    }

    #[test_log::test(tokio::test)]
    async fn test_job_env() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Arc::new(TempRepo::new().await.unwrap());
        let commit1 = repo
            .commit("hello,")
            .await
            .expect("couldn't create test commit");
        let commit2 = repo
            .commit("hello,")
            .await
            .expect("couldn't create test commit");
        let db_dir = TempDir::new().expect("couldn't make temp dir for result DB");

        // Set up 3 tests, one is just there to be depended and the second is
        // there to be _not_ depended on.
        // The third dumps its environment into a file keyed by the test commit.
        let tests = Dag::new([
            Arc::new(Test {
                name: TestName::new("dep"),
                program: OsString::from("bash"),
                args: vec!["-c".into(), OsString::from("true")],
                needs_resources: [
                    (ResourceKey::Worktree, 1),
                    (ResourceKey::UserToken("my_resource".into()), 2),
                ]
                .into(),
                shutdown_grace_period: Duration::from_secs(5),
                cache_policy: CachePolicy::ByCommit,
                config_hash: "0".to_string(),
                depends_on: vec![],
            }),
            Arc::new(Test {
                name: TestName::new("not_dep"),
                program: OsString::from("bash"),
                args: vec!["-c".into(), OsString::from("true")],
                needs_resources: [
                    (ResourceKey::Worktree, 1),
                    (ResourceKey::UserToken("my_resource".into()), 2),
                ]
                .into(),
                shutdown_grace_period: Duration::from_secs(5),
                cache_policy: CachePolicy::ByCommit,
                config_hash: "0".to_string(),
                depends_on: vec![],
            }),
            Arc::new(Test {
                name: TestName::new("my_test"),
                program: OsString::from("bash"),
                args: vec![
                    "-c".into(),
                    OsString::from(format!(
                        "env >> {0:?}/${{LIMMAT_COMMIT}}_env.txt",
                        temp_dir.path()
                    )),
                ],
                needs_resources: [
                    (ResourceKey::Worktree, 1),
                    (ResourceKey::UserToken("my_resource".into()), 2),
                ]
                .into(),
                shutdown_grace_period: Duration::from_secs(5),
                cache_policy: CachePolicy::ByCommit,
                config_hash: "0".to_string(),
                depends_on: vec![TestName("dep".into())],
            }),
        ])
        .expect("couldn't build test DAG");
        let resource_pools = Pools::new(
            [(ResourceKey::Worktree, worktree_resources(&repo, 1).await)]
                .into_iter()
                .chain(
                    [
                        (
                            ResourceKey::UserToken("my_resource".into()),
                            vec![
                                Resource::UserToken("thing1".into()),
                                Resource::UserToken("thing2".into()),
                                Resource::UserToken("thing3".into()),
                            ],
                        ),
                        (
                            ResourceKey::UserToken("other_resource".into()),
                            vec![
                                Resource::UserToken("whing1".into()),
                                Resource::UserToken("whing2".into()),
                                Resource::UserToken("whing3".into()),
                            ],
                        ),
                    ]
                    .into_iter(),
                ),
        );
        let m = Manager::new(
            repo.clone(),
            Arc::new(Database::create_or_open(db_dir.path()).expect("couldn't setup result DB")),
            Arc::new(resource_pools),
            tests,
        );

        m.set_revisions([commit1.clone(), commit2.clone()])
            .await
            .expect("set_revisions failed");
        m.settled().await;

        let env_path = temp_dir.path().join(format!("{}_env.txt", commit2.hash));
        let env_dump = fs::read_to_string(&env_path).unwrap_or_else(|_| {
            panic!(
                "couldn't read env dumped from test script at {}",
                env_path.display()
            )
        });
        let env: HashMap<&str, &str> = env_dump
            .trim()
            .split("\n")
            .filter_map(|line| {
                let parts: Vec<_> = line.splitn(2, "=").collect();
                if parts.len() != 2 {
                    return None;
                }
                Some((parts[0], parts[1]))
            })
            .collect();
        assert_eq!(
            env.get("LIMMAT_ORIGIN"),
            // Ugh I don't fucking know, as_ref as_ref as_ref as_ref just deal
            // with it this is how we write Rust this is good Rust as_ref as_ref.
            Some(repo.path().to_string_lossy().as_ref()).as_ref()
        );
        assert_eq!(
            env.get("LIMMAT_COMMIT").map(|t| CommitHash::new(*t)),
            Some(&commit2.hash).cloned()
        );
        let resource0 = env
            .get("LIMMAT_RESOURCE_my_resource_0")
            .expect("didn't get resource0");
        assert!(
            resource0.starts_with("thing"),
            "bad resource 0: {resource0:?}'"
        );
        let resource1 = env
            .get("LIMMAT_RESOURCE_my_resource_1")
            .expect("didn't get resource1");
        assert!(
            resource1.starts_with("thing"),
            "bad resource 2: {resource1:?}'"
        );
        assert_eq!(env.get("LIMMAT_RESOURCE_my_resource_2"), None);
        assert_eq!(env.get("LIMMAT_RESOURCE_other_resource_0"), None);

        assert_eq!(
            Path::new(env.get("LIMMAT_ARTIFACTS").expect("no LIMMAT_ARTIFACTS")),
            &db_dir
                .path()
                .join(commit2.hash.to_string())
                .join("my_test/artifacts")
        );
        assert_eq!(
            Path::new(
                env.get("LIMMAT_ARTIFACTS_dep")
                    .expect("no LIMMAT_ARTIFACTS")
            ),
            &db_dir
                .path()
                .join(commit2.hash.to_string())
                .join("dep/artifacts")
        );
        assert_eq!(env.get("LIMMAT_ARTIFACTS_notdep"), None);
    }

    #[test_log::test(tokio::test)]
    async fn should_not_start_canceled() {
        let f = TestScriptFixture::builder()
            .num_tests(1)
            .num_worktrees(1)
            .build()
            .await;
        let mut results = f.manager.results();
        let mut commits = Vec::new();
        for _ in 0..5 {
            commits.push(
                f.repo
                    .commit(TestScript::BLOCK_COMMIT_MSG_TAG)
                    .await
                    .expect("couldn't create test commit"),
            );
        }

        // We're gonna start one test and have another become blocked waiting
        // for a worktree. In order to make it deterministic which one gets
        // blocked, we'll do this in two phases.
        f.manager
            .set_revisions(vec![commits[0].clone()])
            .await
            .unwrap();
        // wait for first test to get started.
        expect_notifs_20s(
            &mut results,
            [(
                f.test_case(&commits[0], 0),
                vec![TestStatusMatcher::Enqueued, TestStatusMatcher::Started].into(),
            )],
        )
        .await
        .expect("bad test result");

        // Now we enqueue the test that should block.
        f.manager
            .set_revisions(vec![commits[0].clone(), commits[1].clone()])
            .await
            .unwrap();
        expect_notifs_20s(
            &mut results,
            [(
                f.test_case(&commits[1], 0),
                vec![TestStatusMatcher::Enqueued].into(),
            )],
        )
        .await
        .expect("bad test result");

        // Now we cancel both of those tests.
        f.manager
            .set_revisions::<_, CommitHash>(Vec::new())
            .await
            .unwrap();
        expect_notifs_20s(
            &mut results,
            [
                (
                    f.test_case(&commits[0], 0),
                    vec![TestStatusMatcher::Inconclusive(TestInconclusive::Canceled)].into(),
                ),
                (
                    f.test_case(&commits[1], 0),
                    vec![TestStatusMatcher::Inconclusive(TestInconclusive::Canceled)].into(),
                ),
            ],
        )
        .await
        .expect("bad test result");

        expect_no_more_results(&mut results, &f.manager)
            .await
            .unwrap();

        assert!(!f.scripts[0].was_started(&commits[1].hash));
    }

    #[test_log::test(tokio::test)]
    async fn should_not_cache() {
        let f = TestScriptFixture::builder()
            .num_tests(2)
            .num_worktrees(4)
            .build()
            .await;
        let mut results = f.manager.results();

        // This commit's tests will be terminated by SIGINT if they receive it,
        // which is "an error"
        let with_error = f
            .repo
            .commit(TestScript::BLOCK_COMMIT_MSG_TAG)
            .await
            .expect("couldn't create test commit");
        // This commit's tests will shut down with an error exit-code if
        // SIGTERMed which is normally a test failure. But this should not be
        // cached if the SIGTERM was due to the job being canceled.
        let mut commit_msg = OsString::from(TestScript::BLOCK_COMMIT_MSG_TAG);
        commit_msg.push(TestScript::exit_code_tag(1));
        let with_fail = f
            .repo
            .commit(commit_msg)
            .await
            .expect("couldn't create test commit");

        // Wait until all tests are started.
        f.manager
            .set_revisions(vec![with_error.clone(), with_fail.clone()].clone())
            .await
            .unwrap();
        expect_notifs_20s(
            &mut results,
            [
                (
                    f.test_case(&with_error, 0),
                    vec![TestStatusMatcher::Enqueued, TestStatusMatcher::Started].into(),
                ),
                (
                    f.test_case(&with_error, 1),
                    vec![TestStatusMatcher::Enqueued, TestStatusMatcher::Started].into(),
                ),
                (
                    f.test_case(&with_fail, 0),
                    vec![TestStatusMatcher::Enqueued, TestStatusMatcher::Started].into(),
                ),
                (
                    f.test_case(&with_fail, 1),
                    vec![TestStatusMatcher::Enqueued, TestStatusMatcher::Started].into(),
                ),
            ],
        )
        .await
        .expect("bad test result");

        // Cause one to fail with an error. We take advantage of the fact that
        // this whole tool considers it an "error" when a test exits with a
        // signal instead of exiting with a nonzero code.
        let started_script = f.scripts[0].started(&with_error.hash).await;
        started_script.sigurs1();
        expect_notifs_20s(
            &mut results,
            [(
                f.test_case(&with_error, 0),
                vec![TestStatusMatcher::Inconclusive(TestInconclusive::Error(
                    String::from("terminated by signal 10"),
                ))]
                .into(),
            )],
        )
        .await
        .expect("didn't get error after killing script");
        started_script.reset_started();

        // ... and the others to be canceled.
        f.manager.cancel_running().await.unwrap();
        expect_notifs_20s(
            &mut results,
            [
                (
                    f.test_case(&with_error, 1),
                    vec![TestStatusMatcher::Inconclusive(TestInconclusive::Canceled)].into(),
                ),
                (
                    f.test_case(&with_fail, 0),
                    vec![TestStatusMatcher::Inconclusive(TestInconclusive::Canceled)].into(),
                ),
                (
                    f.test_case(&with_fail, 1),
                    vec![TestStatusMatcher::Inconclusive(TestInconclusive::Canceled)].into(),
                ),
            ],
        )
        .await
        .expect("didn't see test cancellation");

        // They should now get run a second time. i.e. none of the results should have got hashed.
        f.manager
            .set_revisions(vec![with_error.clone(), with_fail.clone()].clone())
            .await
            .unwrap();
        select! {
            _ = sleep(Duration::from_secs(5)) => panic!("error'd test not re-run"),
            _ = f.scripts[0].started(&with_error.hash) => ()
        };
        select! {
            _ = sleep(Duration::from_secs(5)) => panic!("canceled test not re-run"),
            _ = f.scripts[1].started(&with_error.hash) => ()
        };
        select! {
            _ = sleep(Duration::from_secs(5)) => panic!("error'd test not re-run"),
            _ = f.scripts[0].started(&with_fail.hash) => ()
        };
        select! {
            _ = sleep(Duration::from_secs(5)) => panic!("canceled test not re-run"),
            _ = f.scripts[1].started(&with_fail.hash) => ()
        };
    }

    #[test_log::test(tokio::test)]
    async fn should_not_require_worktree() {
        let f = TestScriptFixture::builder()
            .needs_worktree([false, false, false])
            .num_worktrees(1)
            .build()
            .await;
        let test_commit = f
            .repo
            .commit(TestScript::BLOCK_COMMIT_MSG_TAG)
            .await
            .expect("couldn't create test commit");
        let head_commit = f
            .repo
            .commit("woodly doodly")
            .await
            .expect("couldn't create test commit");
        f.manager
            .set_revisions(vec![test_commit.clone()])
            .await
            .unwrap();
        // Even though we have three tests that never finish, and only one
        // worktree, they should all start.
        timeout_5s(join_all(
            f.scripts.iter().map(|s| s.started(&test_commit.hash)),
        ))
        .await
        .expect("not all scripts run for hash");
        // We were testing test_hash but we shouldn't have checked it out, since
        // we don't "own" the worktree.
        assert_eq!(
            f.repo.rev_parse("HEAD").await.unwrap().map(|c| c.hash),
            Some(head_commit.hash)
        );
    }

    #[test_case(OsStr::new(TestScript::BLOCK_COMMIT_MSG_TAG), false ; "blocked¸ shouldn't start")]
    #[test_case(&TestScript::exit_code_tag(1), false ; "failed¸ shouldn't start")]
    #[test_case(&TestScript::exit_code_tag(0), true ; "succeeded¸ should start")]
    #[test_log::test(tokio::test)]
    async fn should_wait_for_dependencies(commit_msg: &OsStr, should_start: bool) {
        let f = TestScriptFixture::builder()
            .dependencies([(1, 0)])
            .num_worktrees(2)
            .build()
            .await;
        let commit = f.repo.commit(commit_msg).await.unwrap();
        f.manager.set_revisions(vec![commit.clone()]).await.unwrap();
        timeout_5s(f.scripts[0].started(&commit.hash))
            .await
            .expect("Initial test did not start");

        if should_start {
            timeout_5s(f.scripts[1].started(&commit.hash))
                .await
                .expect("Depending test did not start");
        } else {
            // Annoying: now we need to check that the second test doesn't get
            // started. But how long do we wait...? Just, some arbitrary time. So
            // sometimes this test can pass even if there's a bug :(
            sleep(Duration::from_secs(1)).await;
            assert_eq!(f.scripts[1].was_started(&commit.hash), should_start);
        }

        // TODO: Test that changes to dependency config hashes invalidates
        // result cache.
    }
}
