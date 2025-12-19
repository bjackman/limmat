use core::fmt;
use core::fmt::{Debug, Display};
use std::ffi::{OsStr, OsString};
use std::ops::Deref;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::process::{self, Command as SyncCommand};
use std::sync::LazyLock;
use std::time::Duration;
use std::{io, str};

use anyhow::anyhow;
use anyhow::{bail, Context};
use async_stream::try_stream;
use colored::control::SHOULD_COLORIZE;
use futures::future::BoxFuture;
use futures::{future::Fuse, select, FutureExt, SinkExt as _, StreamExt as _};
use futures_core::{stream::Stream, FusedFuture};
#[allow(unused_imports)]
use log::{debug, error, info, warn};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::process::OutputExt;
use crate::process::{CommandExt, SyncCommandExt as _};

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct Hash(String);

// My attempt at newtypery for Git IDs. Why is this so damned verbose?
// The answer is that Deref lets you do some stuff on the inner type via
// expressions of the outer type, but it doesn't actually make the outer type
// implement the traits of the inner type. So we have to manually forward all
// those traits.

// A Hash is an ID for referring to an object in a git repository, I think the
// proper name would be ObjectId but... whatever.
impl Hash {
    // Note that this is infallible. That's because having a Hash doesn't
    // guarantee you that the ID refers to an object in an actual repo. Even if
    // we checked that at construction time, it's not possible to enforce that
    // variant going forward. So, you'll just have to do error handling whenever
    // you are dealing with Git objects, like you would with any mutable
    // database.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn abbrev(&self) -> &str {
        &self.0[..12]
    }
}

impl AsRef<OsStr> for Hash {
    fn as_ref(&self) -> &OsStr {
        OsStr::from_bytes(self.0.as_bytes())
    }
}

impl AsRef<str> for Hash {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

impl Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct CommitHash(Hash);

impl CommitHash {
    pub fn new(s: impl Into<String>) -> Self {
        Self(Hash::new(s))
    }
}

impl From<CommitHash> for Hash {
    fn from(h: CommitHash) -> Hash {
        h.0
    }
}

impl Deref for CommitHash {
    type Target = Hash;

    fn deref(&self) -> &Hash {
        &self.0
    }
}

impl AsRef<OsStr> for CommitHash {
    fn as_ref(&self) -> &OsStr {
        self.0.as_ref()
    }
}

impl AsRef<str> for CommitHash {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

impl Display for CommitHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct TreeHash(Hash);

impl TreeHash {
    pub fn new(s: impl Into<String>) -> Self {
        Self(Hash::new(s))
    }
}

impl Deref for TreeHash {
    type Target = Hash;

    fn deref(&self) -> &Hash {
        &self.0
    }
}

impl From<TreeHash> for Hash {
    fn from(h: TreeHash) -> Hash {
        h.0
    }
}

impl AsRef<OsStr> for TreeHash {
    fn as_ref(&self) -> &OsStr {
        self.0.as_ref()
    }
}

impl Display for TreeHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// Worktree represents a git tree, which might be the "main" worktree (in which case it might be
// more clearly refrred to by the name Repo) or some other one.
#[derive(Debug)]
pub struct PersistentWorktree {
    pub path: PathBuf,
    pub git_binary: PathBuf,
}

impl Worktree for PersistentWorktree {
    fn path(&self) -> &Path {
        &self.path
    }

    fn git_binary(&self) -> &Path {
        &self.git_binary
    }
}

#[derive(Debug, Clone)]
pub struct Commit {
    pub hash: CommitHash,
    pub tree: TreeHash,
}

impl Commit {
    #[cfg(test)]
    pub fn arbitrary() -> Self {
        Self {
            hash: CommitHash::new("080b8ecbad3e34e55c5a035af80100f73b742a8d"),
            tree: TreeHash::new("6366d790125291272542a6b40f6fd3400e080821"),
        }
    }
}

impl From<Commit> for CommitHash {
    fn from(val: Commit) -> Self {
        val.hash
    }
}

pub enum LogStyle {
    WithGraph,
    NoGraph,
}

static COMMAND_SEM: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(64));

// Wrapper for a Command, that holds a semaphore for as long as the process
// exists. Just delegates enough methods to allow you to use it without
// letting you drop the semaphore until the process has terminated (which
// hopefully implies the stdio pipes have been closed...).
// This exists to try and avoid running into file descriptor exhaustion, without
// needing any retry logic that would risk creating livelocks.
#[derive(Debug)]
struct GitCommand {
    _permit: SemaphorePermit<'static>,
    command: Command,
}

impl GitCommand {
    fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut GitCommand {
        self.command.arg(arg);
        self
    }

    fn args(&mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> &mut GitCommand {
        self.command.args(args);
        self
    }

    async fn execute(&mut self) -> anyhow::Result<process::Output> {
        self.command.execute().await
    }

    pub async fn output(&mut self) -> io::Result<process::Output> {
        self.command.output().await
    }
}

// Trait's can't have private methods, this is one reason why my
// inheritance-brained idea to use this Worktree kinda like a superclass was not
// a very good one.  This trait is a workaround for that, to avoid linter
// warnings from having a public method return a private type.
trait WorktreePriv: Worktree {
    // Convenience function to create a git command with some pre-filled args.
    // Returns a BoxFuture as an utterly mysterious workaround for what I
    // believe is a compiler bug:
    // https://stackoverflow.com/questions/79350718/one-type-is-more-general-than-the-other-for-osstr-and-tokiospawn?noredirect=1#comment139931420_79350718
    fn git<'a, I, S>(&'a self, args: I) -> BoxFuture<'a, GitCommand>
    where
        I: IntoIterator<Item = S> + Send + 'a,
        S: AsRef<OsStr>,
    {
        (async {
            let mut cmd = Command::new(self.git_binary());
            cmd.current_dir(self.path());
            cmd.args([
                "-c",
                &format!("color.ui={}", SHOULD_COLORIZE.should_colorize()),
            ]);
            cmd.args(args);
            // Separate process group means the child doesn't get SIGINT if the user
            // Ctrl-C's the terminal. We are trusting that git won't get stuck and
            // prevent us from shutting down. The benefit is that we don't get
            // annoying confusing errors on shut down.
            cmd.process_group(0);
            GitCommand {
                _permit: COMMAND_SEM.acquire().await.unwrap(),
                command: cmd,
            }
        })
        .boxed()
    }
}

impl<W: Worktree + ?Sized> WorktreePriv for W {}

// This is a weird kinda inheritance type thing to enable different types of worktree (with
// different fields and drop behaviours) to share the functionality that users actually care about.
// Not really sure if this is the Rust Way or not.
pub trait Worktree: Debug + Sync {
    // Directory where git commands should be run.
    fn path(&self) -> &Path;
    // Path to Git binary.
    fn git_binary(&self) -> &Path;

    async fn lookup_git_dir(&self, rev_parse_arg: &str) -> anyhow::Result<PathBuf> {
        let output = self
            .git(["rev-parse", rev_parse_arg])
            .await
            .execute()
            .await
            .map_err(|e| anyhow!("'git rev-parse {rev_parse_arg}' failed: {e}"))?;
        let mut bytes = output.stdout;
        while bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        Ok(OsStr::from_bytes(&bytes).into())
    }

    // Directory where the main git database lives, shared by all worktrees.
    async fn git_common_dir(&self) -> anyhow::Result<PathBuf> {
        self.lookup_git_dir("--git-common-dir").await
    }

    // Directory where this workrtee's local git database lives.
    // See https://git-scm.com/docs/git-worktree#_details (I haven't read this properly lmao).
    async fn git_dir(&self) -> anyhow::Result<PathBuf> {
        self.lookup_git_dir("--absolute-git-dir").await
    }

    async fn rev_list<S>(&self, range_spec: S) -> anyhow::Result<Vec<CommitHash>>
    where
        S: AsRef<OsStr>,
    {
        let output = self
            .git(["rev-list"])
            .await
            .arg(range_spec)
            .execute()
            .await
            .context("'git rev-list' failed")?;
        // See coment in rev_parse.
        if output.code_not_killed()? == 128 {
            return Ok(vec![]);
        }
        let code = output.status.code().unwrap();
        if code != 0 {
            bail!(
                "failed with exit code {}. stderr:\n{}\nstdout:\n{}",
                code,
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            );
        }
        let out_str: &str = str::from_utf8(&output.stdout).context("non utf-8 rev-list output")?;
        Ok(out_str.lines().map(CommitHash::new).collect())
    }

    async fn checkout(&self, commit: &CommitHash) -> anyhow::Result<()> {
        self.git(["checkout"])
            .await
            .arg(commit)
            .output()
            .await?
            .ok()
            .context(format!(
                "checking out revision {:?} in {:?}",
                commit,
                self.path()
            ))
    }

    async fn log<S, T>(
        &self,
        range_spec: S,
        format_spec: T,
        style: LogStyle,
    ) -> anyhow::Result<Vec<u8>>
    where
        S: AsRef<OsStr>,
        T: AsRef<OsStr>,
    {
        let mut format_arg = OsString::from("--format=");
        format_arg.push(format_spec.as_ref());
        let stdout = self
            .git(match style {
                LogStyle::WithGraph => vec!["log", "--graph"],
                LogStyle::NoGraph => vec!["log"],
            })
            .await
            .args([&format_arg, range_spec.as_ref()])
            .execute()
            .await
            .context(format!(
                "getting graph log for {:?} with format {:?}",
                range_spec.as_ref(),
                format_spec.as_ref(),
            ))?
            .stdout;
        Ok(stdout)
    }

    // Watch for events that could change the meaning of a revspec. When that happens, send an event
    // on the channel with the new resolved spec.
    fn watch_refs<'a>(
        &'a self,
        // TODO: Write this in a way where the user doesn't have to deal with converting to OsStr.
        // (Needs to also work with both owned and reference types I think).
        range_spec: &'a OsStr,
    ) -> anyhow::Result<impl Stream<Item = anyhow::Result<Vec<CommitHash>>> + 'a> {
        // Alternatives considered/attempted:
        //
        // - inotify (also fanotify) doesn't support recursively watching directories, whereas the
        //   notify crate has convenient support for that.
        // - The notify crate has convenient support for sending stuff directly down std::sync::mpsc
        //   channels, and even has support for debouncing those events. However it seems like you
        //   then need to create a whole additional channel if you wanna "map" the events to
        //   something else, i.e. like we wanna call rev_list here.
        //
        // Overall the idea of how to turn this into an async thingy comes from
        // https://github.com/notify-rs/notify/blob/main/examples/async_monitor.rs, I am not sure if
        // this is "real" or toy code that I should not have followed so literally. I do think that
        // this use of futures::executor::block_on is legit - the notify crate spins up a thread
        // under the hood so it's fine to block that thread, and block_on seems to be the proper way
        // to bridge into async code from sync code.
        let (mut tx, mut rx) = futures::channel::mpsc::unbounded();

        let mut watcher = RecommendedWatcher::new(
            move |res| {
                futures::executor::block_on(async {
                    // The documentation is very confusing here, it's hard to figure out why send
                    // would fail. To be my best understanding it just means that the receiver has
                    // been dropped. It's extremely non-obvious whether we can expect this to happen
                    // here. The receiver was declared before the watcher, so the watcher should be
                    // dropped first, right? But, then presumably we move both of them into the
                    // stream object. So, which one gets dropped first? No fucking idea. We'll just
                    // log if an error occurs and maybe it will be helpful for debugging something
                    // else.
                    tx.send(res).await.unwrap_or_else(|err| {
                        info!(
                            "error in git watcher internal send (probably harmless if shutting down): {}",
                            err
                        )
                    });
                })
            },
            Config::default(),
        )?;
        // This logic "debounces" consecutive events within the same 1s window, to avoid thrashing
        // on the downstream logic as Git works its way through changes.
        Ok(try_stream! {
            let git_common_dir = &self.git_common_dir().await.context("getting git common dir")?;
            let git_dir = &self.git_dir().await.context("getting git common dir")?;
            debug!("watching {git_dir:?} and {git_common_dir:?}");
            watcher
                .watch(git_dir, RecursiveMode::Recursive)
                .context("setting up watcher")?;
            if git_dir != git_common_dir {
                watcher
                    .watch(git_common_dir, RecursiveMode::Recursive)
                    .context("setting up watcher")?;
            }

            // Produce an initial update.
            yield self.rev_list(range_spec).await?;

            // Start with an expired timer.
            let mut sleep_fut = pin!(Fuse::terminated());
            loop {
                select! {
                    // Produce an update when the timer expires.
                    () = sleep_fut =>  yield self.rev_list(range_spec).await?,
                    // Ensure the timer is set when we see an update.
                    result = rx.next() => {
                        // There's a bug if the sender has shut down, we should always receive
                        // something.
                        let _ = result.expect("git watcher internal receive error");
                        if sleep_fut.is_terminated() {
                            sleep_fut.set(sleep(Duration::from_secs(1)).fuse());
                        }
                    },
                }
            }
        })
    }

    // None means we successfully looked it up but it didn't exist.
    async fn rev_parse<S>(&self, rev_spec: S) -> anyhow::Result<Option<Commit>>
    where
        S: AsRef<OsStr>,
    {
        // We don't use log_n1 here because we want to check the exit code,
        // that API is designed for users who assume the revision exists.
        let output = {
            let mut cmd = self.git(["log", "-n1", "--format=%H %T"]).await;
            let cmd = cmd.arg(rev_spec);
            cmd.output().await.context("failed to run 'git log -n1'")?
        };
        // Hack: empirically, git returns 128 when the range is invalid, it's not documented
        // but hopefully this is stable behaviour that we're supposed to be able to rely on for
        // this...?
        let exit_code = output.code_not_killed()?;
        if exit_code == 128 {
            return Ok(None);
        }
        if exit_code != 0 {
            bail!("'git log -n1' failed with code {exit_code}");
        }
        let out_string =
            String::from_utf8(output.stdout).context("reading git rev-parse output")?;
        let parts: Vec<&str> = out_string.trim().splitn(2, " ").collect();
        if parts.len() != 2 {
            bail!(
                "Failed to parse result of 'git log -n1' - {out_string:?}\nstderr: {:?}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(Some(Commit {
            hash: CommitHash::new(parts[0]),
            tree: TreeHash::new(parts[1]),
        }))
    }
}

// A worktree that is deleted when dropped. This is kind of a dumb API that just happens to fit this
// project's exact needs. Instead probably Repo::new and this method should return a common trait or
// something.
#[derive(Debug)]
pub struct TempWorktree {
    origin: PathBuf, // Path of repo this was created from.
    temp_dir: TempDir,
    cleaned_up: bool,
    git_binary: PathBuf,
}

impl TempWorktree {
    // Create a worktree based on the origin repo, directly in the temp dir (which should be empty)
    // You must call cleanup on the result, or drop will panic.
    // Cancelling this will ensure we clean up efficiently. If you drop the
    // future without doing that, it has the same consequences as failing to call cleanup.
    pub async fn new<W>(
        ct: &CancellationToken,
        origin: &W,
        temp_dir: TempDir,
    ) -> anyhow::Result<TempWorktree>
    where
        W: Worktree,
    {
        // We create the object now even though it is not actually valid yet.
        // This is a hack to let the drop behaviour kick in immediately even if
        // this constructor is cancelled.
        let zelf = Self {
            origin: origin.path().to_owned(),
            temp_dir,
            cleaned_up: false,
            git_binary: origin.git_binary().to_owned(),
        };
        // Dumb workaround for https://github.com/bjackman/limmat/issues/14
        let mut attempts = 1;
        loop {
            let mut cmd = origin.git(["worktree", "add"]).await;
            let cmd = cmd.arg(zelf.temp_dir.path()).arg("HEAD");
            select! {
                _ = ct.cancelled().fuse() => {
                    zelf.cleanup().await;
                    bail!("canceled")
                },
                res = cmd.execute().fuse() => {
                    match res {
                        Ok(_) => return Ok(zelf),
                        Err(e) => {
                            if attempts >= 5 {
                                bail!("git worktree add failed: {}", e);
                            }
                            attempts += 1;
                        },
                    }
                },
            }
        }
    }

    fn cleanup_cmd(&self) -> Option<SyncCommand> {
        if !self.origin.exists() {
            debug!(
                "Not de-registering worktree at {:?} as origin repo ({:?}) is gone.",
                self.temp_dir.path(),
                self.origin
            );
            return None;
        }
        // We don't create a new process group here, that means if the user
        // Ctrl-C's us while this is going on the Git command will get
        // interrupted too and we'll shut down in a mess. I think that's
        // actually desirable, if it gets to that point the user probably
        // just want us to fuck off and give them their terminal back at
        // whatever cost.
        let mut cmd = SyncCommand::new(self.git_binary());
        // Double --force means remove it even if we were in the middle of
        // creating it.
        cmd.args(["worktree", "remove", "--force", "--force"])
            .arg(self.temp_dir.path())
            .current_dir(&self.origin);
        Some(cmd)
    }

    // Clean up asnchronously, if you don't do this it will be done
    // synchronously in drop (blocking the async runtime and with no opportunity
    // for parallelism) and you will feel like a dumb idiot and your friends
    // will laugh at you.
    pub async fn cleanup(mut self) {
        if let Some(cmd) = self.cleanup_cmd() {
            match Command::from(cmd).execute().await {
                Err(e) => {
                    // This is totally normal, because the constructor creates this
                    // object before being certain the worktree was even created.
                    debug!("Couldn't clean up worktree {:?}: {:?}", &self.temp_dir, e);
                }
                Ok(_) => debug!("Delorted worktree at {:?}", self.temp_dir.path()),
            }
        }

        self.cleaned_up = true;
    }
}

impl Worktree for TempWorktree {
    fn path(&self) -> &Path {
        self.temp_dir.path()
    }

    fn git_binary(&self) -> &Path {
        &self.git_binary
    }
}

impl Drop for TempWorktree {
    fn drop(&mut self) {
        if self.cleaned_up {
            return;
        }
        warn!(
            "TempWorktree was not cleaned up before drop. \
                This is functionally harmless but probably slows things down."
        );
        if let Some(mut cmd) = self.cleanup_cmd() {
            match cmd.execute() {
                Err(e) => {
                    // This is totally normal, because the constructor creates this
                    // object before being certain the worktree was even created.
                    debug!("Couldn't clean up worktree {:?}: {:?}", &self.temp_dir, e);
                }
                Ok(_) => debug!("Delorted worktree at {:?}", self.temp_dir.path()),
            }
        }
    }
}

#[cfg(test)]
pub mod test_utils {

    use super::*;

    #[derive(Debug)]
    pub struct TempRepo {
        temp_dir: TempDir,
        git_binary: PathBuf,
    }

    // Empty repository in a temporary directory, torn down on drop.
    impl TempRepo {
        pub async fn new() -> anyhow::Result<Self> {
            // https://www.youtube.com/watch?v=_MwboA5NIVA
            let zelf = Self {
                temp_dir: TempDir::with_prefix("fixture-").expect("couldn't make tempdir"),
                git_binary: PathBuf::from("git"),
            };
            zelf.git(["init"]).await.execute().await?;
            Ok(zelf)
        }
    }

    impl Worktree for TempRepo {
        fn path(&self) -> &Path {
            self.temp_dir.path()
        }

        fn git_binary(&self) -> &Path {
            &self.git_binary
        }
    }

    pub trait WorktreeExt: Worktree {
        // timestamp is used for both committer and author. This ought to make
        // commit hashes deterministic.
        async fn commit<S>(&self, message: S) -> anyhow::Result<Commit>
        where
            S: AsRef<OsStr>,
        {
            self.git(["commit", "-m"])
                .await
                .arg(message)
                .arg("--allow-empty")
                .execute()
                .await
                .context("'git commit' failed")?;
            // Doesn't seem like there's a safer way to do this than commit and then retroactively parse
            // HEAD and hope nobody else is messing with us.
            self.rev_parse("HEAD")
                .await?
                .ok_or(anyhow!("no HEAD after committing"))
        }

        async fn merge(&self, parents: &[CommitHash]) -> anyhow::Result<Commit> {
            self.git(["merge", "-m", "merge commit"])
                .await
                .args(parents)
                .execute()
                .await
                .context("'git commit' failed")?;
            self.rev_parse("HEAD")
                .await
                .context("getting commit after merge")?
                .context("no HEAD after merge")
        }
    }

    impl<W: Worktree> WorktreeExt for W {}
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;

    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn test_new_gitdir_notgit() {
        let tmp_dir = TempDir::new().expect("couldn't make tempdir");
        let wt = PersistentWorktree {
            path: tmp_dir.path().to_path_buf(),
            git_binary: PathBuf::from("git"),
        };
        assert!(
            wt.git_common_dir().await.is_err(),
            "opening repo with no .git didn't fail"
        );
    }

    #[tokio::test]
    async fn test_new_gitdir_file_notgit() {
        let tmp_dir = TempDir::new().expect("couldn't make tempdir");
        {
            let mut bogus_git_file =
                File::create(tmp_dir.path().join(".git")).expect("couldn't create .git");
            write!(bogus_git_file, "no no no").expect("couldn't write .git");
        }
        let wt = PersistentWorktree {
            path: tmp_dir.path().to_path_buf(),
            git_binary: PathBuf::from("git"),
        };
        assert!(
            wt.git_common_dir().await.is_err(),
            "opening repo with bogus .git file didn't fail"
        );
    }
}
