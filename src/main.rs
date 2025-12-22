use anyhow::{anyhow, bail, Context};
use clap::{arg, Parser as _, Subcommand, ValueEnum};
use config::{Config, ParsedConfig};
use dag::{Dag, GraphNode as _};
use database::{Database, DatabaseEntry, DatabaseOutput, LookupResult};
use flexi_logger::{detailed_format, Cleanup, Criterion, FileSpec, Logger, Naming};
use futures::future::join_all;
use futures::StreamExt;
use git::{Commit, PersistentWorktree, TempWorktree};
use http::Ui;
use log::{debug, error, warn};
use nix::sys::utsname::uname;
use resource::Pools;
use resource::{Resource, ResourceKey};
use std::borrow::Borrow as _;
use std::cmp::min;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Display;
use std::io::{stdout, Stdout};
use std::path::{absolute, PathBuf};
use std::pin::pin;
use std::process::{ExitCode, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::{env, fmt, fs, str};
use tempfile::TempDir;
use test::{base_job_env, Manager, TestCase, TestCaseId, TestJob, TestJobBuilder, TestName};
use test::{DepDatabaseEntries, Test};
use tokio::select;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;
use util::{DisplayablePathBuf, ErrGroup};

use crate::git::Worktree;
use crate::terminal::TerminalSizeWatcher;

mod config;
mod dag;
mod database;
mod flock;
mod git;
mod http;
mod process;
mod resource;
mod terminal;
mod test;
mod text;
mod ui;
mod util;

#[cfg(test)]
mod test_utils;

#[derive(clap::Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    // TODO: Don't require valid utf-8 strings here, OsStrings shoud be fine. But
    // https://stackoverflow.com/questions/76341332/clap-default-value-for-pathbuf
    #[arg(short, long, default_value_t = {".".to_string()}, global = true)]
    repo: String,
    /// Path to TOML config file. Default is $LIMMAT_CONFIG if non-empty,
    /// or ./limmat.toml if it exists, or ./.limmat.toml if it exists
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    /// Directory where results will be stored.
    #[arg(long, default_value_t = default_result_db(), global = true)]
    result_db: DisplayablePathBuf,
    /// Filename prefix for temporary worktrees.
    #[arg(long, default_value_t = {"limmat-worktree".to_string()}, global = true)]
    worktree_prefix: String,
    /// Directory (must exist) to create temporary worktrees in.
    #[arg(long, default_value_t = {env::temp_dir().to_string_lossy().into_owned()}, global = true)]
    worktree_dir: String,
    /// Git binary - default will use $PATH.
    #[arg(long, default_value_t = {DisplayablePathBuf("git".into())}, global = true)]
    git_binary: DisplayablePathBuf,
    /// Regexes of tests to skip.
    #[arg(long, global = true)]
    skip_test: Vec<String>,
    /// Regexes of tests to include. If specified, only tests matching these regexes will be run.
    #[arg(long, global = true)]
    tests: Vec<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args, Debug)]
struct WatchArgs {
    /// Socket address in the form "$ip:$port" to listen on for serving files
    /// over HTTP. For example "127.0.0.1:8080" or ""[::1]:1234". Set the port
    /// to 0 to let the OS pick a port for us.
    #[arg(long, default_value_t = {"0.0.0.0:0".to_string()})]
    http_sockaddr: String,
    /// Hostname to use for HTTP URLs
    #[arg(long, default_value_t = default_hostname())]
    hostname: String,
    /// Base of range to test. Will test commits between this (exclusive) and
    /// HEAD (inclusive). Whenever HEAD changes, this string will be re-evaluated
    /// to find the base of the range.
    base: String,
}

static PROJECT_DIRS: LazyLock<directories::ProjectDirs> = LazyLock::new(|| {
    directories::ProjectDirs::from("", "", "limmat").expect("couldn't find user data dir")
});

fn default_result_db() -> DisplayablePathBuf {
    DisplayablePathBuf(PROJECT_DIRS.data_local_dir().to_owned())
}

fn default_hostname() -> String {
    uname()
        .expect("couldn't get nodename")
        .nodename()
        .to_owned()
        .into_string()
        .expect("hostname wasn't utf-8")
}

// Returns the path we should look for the the config. Although this is
// influenced by the existence of files, it doesn't guarantee that the returned
// file exists.
fn find_config(config_arg: &Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = config_arg {
        return Ok(path.clone()); // (Might not exist)
    }
    if let Some(path) = env::var_os("LIMMAT_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    let limmat_dot_toml = PathBuf::from("./limmat.toml");
    if limmat_dot_toml.exists() {
        return Ok(limmat_dot_toml);
    }
    let dot_limmat_dot_toml = PathBuf::from("./.limmat.toml");
    if dot_limmat_dot_toml.exists() {
        return Ok(dot_limmat_dot_toml);
    }
    bail!("Neither config nor $LIMMAT_CONFIG were set. No ./limmat.toml or ./.limmat.toml found");
}

#[derive(clap::Args, Debug)]
struct TestArgs {
    /// Name of the test to run, per the "name" field in the config file.
    test: String,
}

// Args common to commands that get results from the database
#[derive(clap::Args, Debug, Clone)]
struct DatabaseLookupArgs {
    /// Name of the test, per the "name" field in the config file.
    test: String,
    /// Whether to run the test if the result is not in the database.
    #[arg(long, default_value_t = false)]
    run: bool,
    /// Revision to test. Any git revspec is fine.
    rev: String,
}

#[derive(clap::Args, Debug)]
struct GetArgs {
    #[command(flatten)]
    lookup_args: DatabaseLookupArgs,
    /// Which output from the job do we want?
    #[arg(default_value_t = GetOutput::Stdout)]
    output: GetOutput,
}

#[derive(Clone, ValueEnum, Debug)]
enum GetOutput {
    Stdout,
    Stderr,
}

impl Display for GetOutput {
    fn fmt(&self, w: &mut fmt::Formatter) -> fmt::Result {
        write!(
            w,
            "{}",
            match self {
                Self::Stdout => "stdout",
                Self::Stderr => "stderr",
            }
        )
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    /// The main command. Watch a repository and run tests whenever the revision
    /// range changes.
    Watch(WatchArgs),
    /// Run a one-shot test in the specified repo. Do not cache the results.
    Test(TestArgs),
    /// EXPERIMENTAL: Get the path of a test's output in the result database.
    /// Returns exit code 50 if the result doesn't exist.
    Get(GetArgs),
    /// Get the path to the artifacts for a given test. Returns exit code 50
    /// if the result doesn't exist.
    Artifacts(DatabaseLookupArgs),
}

// Kitchen-sink object for global shit.
struct Env {
    config: ParsedConfig,
    repo: Arc<git::PersistentWorktree>,
    database: Arc<Database>,
    worktree_builder: WorktreeBuilder,
}

// Fallback instead of https://github.com/Stebalien/tempfile/pull/308
struct WorktreeBuilder {
    prefix: OsString,
    parent_dir: PathBuf,
}

impl WorktreeBuilder {
    pub fn build(&self) -> anyhow::Result<TempDir> {
        tempfile::Builder::new()
            .prefix(&self.prefix)
            .tempdir_in(&self.parent_dir)
            .context("creating temp dir for worktree")
    }
}

// This is the main loop of the program. Take notifications from the Git tree,
// feed them to the test manager, feed the test manager's results to the status
// viewer (basically the UI).
async fn watch_loop(
    cancellation_token: CancellationToken,
    test_manager: Arc<test::Manager<PersistentWorktree>>,
    mut ui: ui::StatusViewer<PersistentWorktree, Stdout>,
    range_spec: OsString,
    repo: Arc<PersistentWorktree>,
) -> anyhow::Result<()> {
    let mut revs_stream = pin!(repo.watch_refs(&range_spec)?);
    let mut notifs = test_manager.results();

    let size_watcher = TerminalSizeWatcher::new()?;
    let mut resizes = pin!(size_watcher.resizes());

    loop {
        select! {
            // We can sometimes get spammed with huge numbers of notifications; ensure that
            // ctrl-C is processed first.
            biased;

            _ =  cancellation_token.cancelled() => {
                break;
            }
            // TODO: It's dumb that we have two different types of communication here (one exposes
            // the channel, one implements Stream).
            revs = revs_stream.next() => {
                // TODO: figure out if/how this can actually fail.
                let mut revs = revs.expect("revset stream terminated")?;
                // When we accidentally get run on a massive range,
                // set_revisions can take a long time, which with this
                // simplistic loop approach can block the UI which is annoying.
                // Since the user isn't gonna get anything useful out of Limmat
                // in that situation anyway, just discard revisions that might
                // cause this problem.
                if revs.len() > 1024 {
                    warn!("Got %d revisions in range. Will only test 1024");
                }
                revs.truncate(1024);
                // Paying for a pointless clone here so we can do set_revisions
                // (mostly just kicks off background stuff) before awaiting the
                // UI reset (does synchronhous work).
                test_manager.set_revisions(revs).await.context("setting revisions to test")?;
                ui.set_range(&range_spec).await.context("resetting status viewer")?;
                ui.repaint(&size_watcher.size()).context("error painting status to stdout")?;
            },
            notif = notifs.recv() => {
                let notif = match notif {
                    Ok(n) => n,
                    Err(RecvError::Lagged(num_dropped)) => {
                        error!("Dropped {num_dropped} notifications.");
                        continue;
                    },
                    // https://github.com/rust-lang/futures-rs/issues/1857
                    // AFAICS there is no way to encode a stream that never terminates.
                    Err(RecvError::Closed) => { panic!("notification stream terminated"); },
                };
                ui.update(notif);
                ui.repaint(&size_watcher.size()).context("error painting status to stdout")?;
            },
            _ = resizes.next() => {
                ui.repaint(&size_watcher.size()).context("error painting status to stdout")?;
            },
        }
    }
    // Break out of the TUI.
    drop(ui);
    eprintln!("Got shutdown signal, terminating jobs and waiting");
    test_manager
        .cancel_running()
        .await
        .context("cancelling tests")?;
    eprintln!("Shutting down - waiting for jobs to terminate");
    // Ensure jobs are shut down before we delort stuff etc.
    test_manager.settled().await;
    Ok(())
}

async fn watch(
    env: Env,
    cancellation_token: CancellationToken,
    watch_args: WatchArgs,
) -> anyhow::Result<()> {
    let mut eg = ErrGroup::new(cancellation_token.clone());

    // Create HTTP server, to serve the result artifacts to the user when they
    // click terminal hyperlinks.
    let listener = tokio::net::TcpListener::bind(watch_args.http_sockaddr.clone())
        .await
        .context("setting up HTTP server")?;
    let ui = Ui::new(
        watch_args.hostname.clone(),
        listener,
        env.database.base_dir.clone(),
        format!(
            "Limmat | {}",
            absolute(env.repo.path())
                .context("error getting absolute path of repo")?
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or("<unknown>".into())
        ),
    );
    let result_url_base = ui.result_url_base()?;
    let home_url = ui.home_url()?;
    let ui_state = ui.state();
    eg.spawn(ui.serve(cancellation_token.child_token()));

    // Set up the test manager, which is the weirdly-scoped god-object that
    // orchestrates test jobs.
    let test_manager = Arc::new(Manager::new(
        env.repo.clone(),
        &env.config.source_path,
        env.database,
        env.config.resource_pools.clone(),
        env.config.tests,
    ));

    // Set up the UI, which shows the user what's going on in the terminal.
    let ui = ui::StatusViewer::new(
        env.repo.clone(),
        stdout(),
        ui_state,
        result_url_base,
        home_url,
    );

    // Kick off creation of the worktrees that the test manager will run jobs in.
    //
    // Once we've done this, we can no longer return from this function until
    // we've also cleaned the worktrees up. This is stinky and gross. AFAICT
    // async Rust just doesn't have a solution for that at all.
    //
    // TODO: This doesn't work if there are no commits in the repository. Not sure I care about
    // this, but the solution would be to create the worktrees ondemand, when we have a revision we
    // are actually trying to test. That might be a good idea anyway, so probably it's preferable to
    // just do that for its own sake and leave the empty-repo problem as a nice freebie.
    eprintln!("Creating {} worktrees...", env.config.num_worktrees);
    for _ in 0..env.config.num_worktrees {
        let repo = env.repo.clone();
        let ct = cancellation_token.child_token();
        let resource_pools = env.config.resource_pools.clone();
        let dir = env.worktree_builder.build()?;
        eg.spawn(async move {
            let worktree = TempWorktree::new::<PersistentWorktree>(&ct, repo.as_ref(), dir).await?;
            resource_pools.add([(ResourceKey::Worktree, Resource::Worktree(worktree))]);
            Ok(())
        });
    }

    // DO THE THING.
    eg.spawn(watch_loop(
        cancellation_token.child_token(),
        test_manager.clone(),
        ui,
        format!("{}..HEAD", watch_args.base).into(),
        env.repo,
    ));

    let end_result = eg.wait().await;

    // Now we have to remember to clean up before returning the result :/
    eprintln!("Tearing down worktrees...");
    join_all(
        Arc::into_inner(test_manager)
            .expect("leaked test manager reference")
            .into_resource_pools()
            .try_remove_worktrees()
            .map(|w| w.cleanup()),
    )
    .await;

    end_result
}

async fn ensure_job_success(
    database: Arc<Database>,
    resource_pools: Arc<Pools>,
    job: TestJob,
    origin_worktree: PathBuf,
) -> anyhow::Result<Arc<DatabaseEntry>> {
    let name = job.test_name().to_owned();
    let db_entry = job
        .run(database, resource_pools.as_ref(), &origin_worktree)
        .await
        .with_context(|| format!("running dependency job {name}"))?;
    if db_entry.exit_code() != 0 {
        bail!(
            "dependency job {name} failed with exit code {}",
            db_entry.exit_code()
        );
    }
    Ok(db_entry)
}

// Run a set of tests at a given version, in worktrees, in parallel, unless
// there's already a result in the database. Error if any fail.
async fn ensure_tests_run(
    env: &Env,
    cancellation_token: CancellationToken,
    tests: Vec<&Arc<Test>>,
    rev: &Commit,
) -> anyhow::Result<DepDatabaseEntries> {
    let tests = tests.into_iter();
    let num_worktrees = min(
        env.config.num_worktrees,
        tests.clone().filter(|t| t.needs_worktree()).count(),
    );

    let job_env = Arc::new(base_job_env(env.repo.path(), &env.config.source_path));

    // Get the graph of tests we need to run as dependencies.
    // This is kinda inefficient: we're building a new Dag based on a subset of
    // the old one, so the validation in the constructor is not strictly
    // necessary. There are two levels of optimisation we could do here:
    // 1. We could add a way to build the new Dag from the TopDown iterator
    //    or some derivative of it, and use the type safety to just skip
    //    validation
    // 2. We could build the subset graph in place, i.e. totally skip making a
    //    new graph and instead just logicall remove the nodes we don't need.
    let jobs = Dag::new(
        // TODO: Would be nice to have an _into thing so we can avoid this clone.
        tests.map(|t| TestCase::new(rev.clone(), t.clone())),
    )
    .context("setting up dependency test graph")?
    .bottom_up()
    .try_fold(
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
                cancellation_token.clone(),
                // TODO: it would be nice if we had an into_ variant of
                // the bottom_up so we didn't need this clone.
                test_case.clone(),
                job_env.clone(),
                wait_for,
            )
            .build();
            jobs.insert(test_case.id().borrow().to_owned(), job);
            Ok(jobs)
        },
    )?;

    // Kick off creation of the worktrees that the dep jobs will run in.
    // This is horribly copy-pasted from watch. I dunno, I can't figure out how
    // to fix that without insanely complex async stuff.
    let mut eg = ErrGroup::new(cancellation_token.clone());
    for _ in 0..num_worktrees {
        let repo = env.repo.clone();
        let ct = cancellation_token.child_token();
        let resource_pools = env.config.resource_pools.clone();
        let dir = env.worktree_builder.build()?;
        eg.spawn(async move {
            let worktree = TempWorktree::new::<PersistentWorktree>(&ct, repo.as_ref(), dir).await?;
            resource_pools.add([(ResourceKey::Worktree, Resource::Worktree(worktree))]);
            Ok(())
        });
    }

    let dep_db_entries = Arc::new(Mutex::new(HashMap::new()));
    for (_, job) in jobs {
        let dep_db_entries = dep_db_entries.clone();
        let db = env.database.clone();
        let resource_pools = env.config.resource_pools.clone();
        let repo_path = env.repo.path().to_owned();
        eg.spawn(async move {
            let test_name = job.test_name().clone();
            let db_entry = ensure_job_success(db, resource_pools, job, repo_path).await?;
            dep_db_entries.lock().unwrap().insert(test_name, db_entry);
            Ok(())
        });
    }

    let result = eg.wait().await;

    // Now we have to remember to clean up before returning the result :/
    join_all(
        env.config
            .resource_pools
            .try_remove_worktrees()
            .map(|w| w.cleanup()),
    )
    .await;

    result?;
    Ok(Arc::into_inner(dep_db_entries)
        .expect("leaked Arc reference")
        .into_inner()
        .unwrap())
}

async fn test(
    env: Env,
    cancellation_token: CancellationToken,
    test_args: &TestArgs,
) -> anyhow::Result<()> {
    let test_name = TestName::new(test_args.test.clone());
    // So we can cache the results in the database, the dependency jobs will be run at HEAD.
    let head = env
        .repo
        .rev_parse("HEAD")
        .await
        .context("failed to look up HEAD commit")?
        .ok_or(anyhow!("no HEAD commit - repo empty?"))?;

    // Only need worktrees for the tests that needs worktrees.
    let dep_tests: Vec<&Arc<Test>> = env
        .config
        .tests
        .top_down_from(&test_name)
        .ok_or(anyhow!("no such test {:?}", test_name.to_string()))?
        // Exclude the main test, we'll run that separately.
        .skip(1)
        .collect();

    let mut dep_db_entries = HashMap::new();
    if !dep_tests.is_empty() {
        eprintln!("Running {} dependency jobs...", dep_tests.len());
        dep_db_entries =
            ensure_tests_run(&env, cancellation_token.child_token(), dep_tests, &head).await?;
        eprintln!("Dependency jobs complete.");
    }

    let test = env.config.tests.node(&test_name).unwrap();
    let test_case = TestCase::new(head.clone(), test.clone());
    let mut needs_resources = test_case.test.needs_resources.clone();
    let job = TestJobBuilder::new(
        cancellation_token.clone(),
        test_case,
        Arc::new(base_job_env(env.repo.path(), &env.config.source_path)),
        Vec::new(), // wait_for
    )
    .build();
    // Doesn't need a worktree, it's gonna do it live and direct in the main tree.
    needs_resources.remove(&ResourceKey::Worktree);
    let resources = env.config.resource_pools.get(needs_resources).await;
    let output_dir = TempDir::with_prefix("limmat-output-")?.into_path();
    eprintln!(
        "Test artifacts will be stored under {}",
        output_dir.display()
    );
    let output = DatabaseOutput::ephemeral(
        output_dir,
        Stdio::inherit(),
        Stdio::inherit(),
        test.separate_outputs,
    )
    .await?;
    let db_entry = job
        .run_with(env.repo.path(), &resources, output, dep_db_entries)
        .await?;
    eprintln!("Finished: {}", db_entry.result());
    if db_entry.result().exit_code == 0 {
        Ok(())
    } else {
        bail!("Test failed with exit code {}", db_entry.result().exit_code)
    }
}

// Returns Ok(None) if nothing found and --run wasn't set.
// Otherwise returns the database entry or an error.
async fn lookup(
    env: Env,
    cancellation_token: CancellationToken,
    lookup_args: &DatabaseLookupArgs,
) -> anyhow::Result<Option<DatabaseEntry>> {
    let test_name = TestName::new(lookup_args.test.clone());
    let rev = env
        .repo
        .rev_parse(&lookup_args.rev)
        .await
        .context("error looking up commit")?
        .ok_or_else(|| anyhow!("revision {:?} not found", lookup_args.test))?;

    if lookup_args.run {
        let tests: Vec<&Arc<Test>> = env
            .config
            .tests
            .top_down_from(&test_name)
            .ok_or(anyhow!("no such test {:?}", test_name.to_string()))?
            .collect();

        // Write to stderr so the output can just be the path, for scripting.
        eprintln!("Running {} tests...", tests.len());
        ensure_tests_run(&env, cancellation_token.child_token(), tests, &rev).await?;
        eprintln!("Tests complete");
    }

    let test = env
        .config
        .tests
        .node(&test_name)
        .ok_or(anyhow!("no such test {:?}", test_name.to_string()))?;
    match env
        .database
        .lookup(&TestCase::new(rev.clone(), test.clone()))
        .await
        .context("database lookup")?
    {
        LookupResult::FoundResult(e) => Ok(Some(e)),
        LookupResult::YouRunIt(_) => {
            if lookup_args.run {
                bail!(
                    "no database entry for test {:?} at revision {:?} after running ({})",
                    test_name.to_string(),
                    lookup_args.rev,
                    rev.hash
                )
            } else {
                Ok(None)
            }
        }
    }
}

const NO_RESULT_FOUND_EXIT_CODE: u8 = 50;

async fn get(
    env: Env,
    cancellation_token: CancellationToken,
    get_args: GetArgs,
) -> anyhow::Result<ExitCode> {
    let db_entry = match lookup(env, cancellation_token, &get_args.lookup_args).await? {
        None => return Ok(ExitCode::from(NO_RESULT_FOUND_EXIT_CODE)),
        Some(e) => e,
    };
    match get_args.output {
        GetOutput::Stdout => println!("{}", db_entry.stdout_path().display()),
        GetOutput::Stderr => println!("{}", db_entry.stderr_path().display()),
    }
    Ok(ExitCode::SUCCESS)
}

async fn artifacts(
    env: Env,
    cancellation_token: CancellationToken,
    lookup_args: DatabaseLookupArgs,
) -> anyhow::Result<ExitCode> {
    let db_entry = match lookup(env, cancellation_token, &lookup_args).await? {
        None => return Ok(ExitCode::from(NO_RESULT_FOUND_EXIT_CODE)),
        Some(e) => e,
    };
    println!("{}", db_entry.artifacts_dir().display());
    Ok(ExitCode::SUCCESS)
}

const MEGABYTE: u64 = 1024 * 1024;

// Hack so we can use anyhow::Result infrastructure for convenient coding but
// also control the exit code directly in some cases: return a result - if it's
// an error we just use the default error exit code.
async fn do_main() -> anyhow::Result<ExitCode> {
    let mut logger = Logger::try_with_env_or_str("debug")?.format(detailed_format);
    logger = match env::var_os("LIMMAT_LOGFILE") {
        Some(path) => logger.log_to_file(
            FileSpec::try_from(&path)
                .with_context(|| format!("configuring logging to {:?}", path))?,
        ),
        None => {
            let log_dir = PROJECT_DIRS
                .state_dir()
                .unwrap_or(PROJECT_DIRS.data_local_dir());
            logger
                .log_to_file(FileSpec::default().directory(log_dir))
                .create_symlink(log_dir.join("latest.log"))
                .rotate(
                    Criterion::Size(64 * MEGABYTE),
                    Naming::TimestampsDirect,
                    Cleanup::KeepLogFiles(10),
                )
        }
    };
    logger.start()?;

    // Set up shutdown first, to ensure we correctly handle early signals.
    // It seems extremely difficult to use Tokio's ctrl_c API for this correctly
    // (It only installs the handler the first time the signal gets polled)
    // so we just go directly for the "interrupt" thing, which might not work on
    // Windows, not sure. (I don't think this code is likely to work on Windows
    // anyway).
    let cancellation_token = CancellationToken::new();
    let mut sigint = signal(SignalKind::interrupt()).context("registering SIGINT handler")?;
    let token = cancellation_token.clone();
    tokio::spawn(async move {
        sigint.recv().await;
        token.cancel()
    });

    let args = Args::parse();
    debug!("args: {:?}", &args);
    let config_path = find_config(&args.config)?;
    let config_content = fs::read_to_string(&config_path).context("couldn't read config")?;
    debug!("config:\n{}", &config_content);
    let config: Config = toml::from_str(&config_content).context("couldn't parse config")?;
    let config = ParsedConfig::new(
        config,
        config_path,
        args.skip_test.iter().map(|s| s.as_str()),
        args.tests.iter().map(|s| s.as_str()),
    )?;

    let repo = git::PersistentWorktree {
        path: args.repo.to_owned().into(),
        git_binary: args.git_binary.clone().into(),
    };
    // Check repo is valid.
    repo.git_common_dir()
        .await
        .context(format!("opening repo {}", args.repo))?;

    let env = Env {
        config,
        repo: Arc::new(repo),
        database: Arc::new(Database::create_or_open(&args.result_db)?),
        worktree_builder: WorktreeBuilder {
            prefix: args.worktree_prefix.into(),
            parent_dir: args.worktree_dir.into(),
        },
    };

    match args.command {
        Command::Get(get_args) => get(env, cancellation_token, get_args).await,
        Command::Artifacts(lookup_args) => artifacts(env, cancellation_token, lookup_args).await,
        c => {
            match c {
                Command::Watch(watch_args) => watch(env, cancellation_token, watch_args).await,
                Command::Test(ref test_args) => test(env, cancellation_token, test_args).await,
                _ => panic!("wtf"),
            }?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    match do_main().await {
        Err(err) => {
            eprintln!("Fatal error: {:#}", err);
            ExitCode::FAILURE
        }
        Ok(code) => code,
    }
}
