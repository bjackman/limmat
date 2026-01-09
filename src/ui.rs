use std::{collections::HashMap, ffi::OsStr, io::Write, mem, sync::Arc};

use ansi_control_codes::control_sequences::{CUP, ED};
use anyhow::{self, bail, Context as _};
use colored::Colorize;
use futures::future::try_join;
use lazy_static::lazy_static;
#[allow(unused_imports)]
use log::debug;
use regex::Regex;

use crate::{
    database::Database,
    git::{CommitHash, LogStyle, Worktree},
    http::UiState,
    test::{Notification, TestCase, TestInconclusive, TestName, TestStatus},
    text::{Class, Line, Span, Text},
    util::{Rect, ResultExt as _},
};

struct TrackedTestCase {
    test_case: TestCase,
    status: TestStatus,
}

// Inner string key is test name. Here we awkwardly store this as a
// two-level map instead of a flat one by TestCaseId, because that
// conveniently lets us grab all the TestCases for a given commit when
// rendering the output.
type TrackedCases = HashMap<CommitHash, HashMap<TestName, TrackedTestCase>>;

// Updates the awkward nested hashmap to reflect a new notification coming in.
// Standalone function for convenient use in tests.
fn update_tracked_cases(tracked_cases: &mut TrackedCases, notif: Arc<Notification>) {
    let commit_statuses = tracked_cases
        .entry(notif.test_case.commit_hash.clone())
        .or_default();
    commit_statuses.insert(
        notif.test_case.test.name.clone(),
        TrackedTestCase {
            test_case: notif.test_case.clone(),
            status: notif.status.clone(),
        },
    );
}

// Tracks the status of the tests being run by observing the notification
// stream.
pub struct StatusViewer<W: Worktree, O: Write> {
    repo: Arc<W>,
    tracked_cases: TrackedCases,
    output_buf: OutputBuffer,
    output: O,
    web_ui: Arc<UiState>,
    result_url_base: String,
    home_url: String,
}

// This ought to be private to StatusViewer::reset, rust just doesn't seem to
// let you do that.
lazy_static! {
    static ref COMMIT_HASH_REGEX: Regex = Regex::new("[0-9a-z]{40,}").unwrap();
    static ref GRAPH_COMPONENT_REGEX: Regex = Regex::new(r"[\\/\*]").unwrap();
}

impl<W: Worktree, O: Write> StatusViewer<W, O> {
    // Construct a UI that writes to to the given outut. The URL
    // base is used to generate hyperlinks to test results.
    pub fn new(
        repo: Arc<W>,
        output: O,
        web_ui: Arc<UiState>,
        result_url_base: impl Into<String>,
        home_url: impl Into<String>,
    ) -> Self {
        Self {
            repo,
            tracked_cases: HashMap::new(),
            output_buf: OutputBuffer::empty(),
            output,
            web_ui,
            result_url_base: result_url_base.into(),
            home_url: home_url.into(),
        }
    }

    // Informs the UI of the range of tests that we expect to be testing.
    pub async fn set_range(&mut self, range_spec: &OsStr) -> anyhow::Result<()> {
        // This should eventually be configurable.
        let log_format =
            "%Cred%h%Creset -%C(yellow)%d%Creset %s %Cgreen(%cr) %C(bold blue)<%an>%Creset";

        self.output_buf = OutputBuffer::new(&self.repo, range_spec, log_format).await?;
        Ok(())
    }

    // Absorb a notification.
    pub fn update(&mut self, notif: Arc<Notification>) {
        update_tracked_cases(&mut self.tracked_cases, notif);
    }

    // Update the UI by writing it to the output with fancy terminal escape
    // codes to overwrite what was previously written.
    pub fn repaint(&mut self, term_size: &Rect) -> anyhow::Result<()> {
        let render = self
            .output_buf
            .render(&self.tracked_cases, &self.result_url_base);

        self.web_ui.set_log_buf(render.html_pre());

        // Enter alternate screen. Dunno why ansi-control-codes doesn't have
        // this. This isn't really how I wanted this UI to work. But
        // implementing what I really wanted turns out to be really fucking
        // fiddly and unsatisfying and boring, I just don't care enough.
        // This is idempotent so we just do it every time.
        writeln!(self.output, "\x1B[?1049h")?;
        // Move cursor to top left and erase the display.
        write!(&mut self.output, "{}{}", CUP(Some(0), Some(0)), ED(None))?;
        let truncated = Text::from_iter(
            render
                .into_lines()
                // I'm not sure why we need to subtract 3 here instead of 1 (for
                // the line we print below). Something causes the cursor to
                // bounce around and leave two empty lines at the bottom. Don't
                // care, it's too boring to figure this stuff out, lmao.
                .take(term_size.rows.saturating_sub(3))
                .map(|l| l.truncate_graphemes(term_size.cols)),
        );
        write!(&mut self.output, "{}", truncated.ansi())?;
        writeln!(
            &mut self.output,
            "Web UI: {}",
            self.home_url.bold().on_blue()
        )?;

        Ok(())
    }
}

impl<W: Worktree, O: Write> Drop for StatusViewer<W, O> {
    fn drop(&mut self) {
        writeln!(self.output, "\x1B[?1049l").or_log_error("Couldn't exit alternate screen");
    }
}

// Helper for OutputBuffer - just the graph bit of the git log --graph output.
struct GraphBuffer {
    raw_buf: String, // Output straight from Git.
}

impl GraphBuffer {
    pub async fn new(
        repo: &Arc<impl Worktree>,
        range_spec: impl AsRef<OsStr>,
    ) -> anyhow::Result<Self> {
        // Get the raw buffer.
        let raw_buf = repo
            .log(range_spec.as_ref(), "%H\n", LogStyle::WithGraph)
            .await?;
        // OsStr doesn't have a proper API, luckily we can expect utf-8.
        let raw_buf = String::from_utf8(raw_buf)
            .map_err(|_err| anyhow::anyhow!("got non-utf8 output from git log"))?;
        Ok(Self { raw_buf })
    }

    // Parse pairs of commit hash and chunks from the buffer, split by line.
    pub fn chunks(&self) -> anyhow::Result<Vec<(CommitHash, Vec<&str>)>> {
        // Separate out chunks into separate vecs of lines.
        let mut cur_chunk = Vec::<&str>::new();
        let mut raw_chunks = Vec::<Vec<&str>>::new();
        for line in self.raw_buf.trim().split('\n') {
            // --graph uses * to represent a node in the DAG.
            if line.contains('*') && !cur_chunk.is_empty() {
                raw_chunks.push(mem::take(&mut cur_chunk));
            }
            if !line.is_empty() {
                cur_chunk.push(line);
            }
        }
        if !cur_chunk.is_empty() {
            raw_chunks.push(cur_chunk);
        }

        // Now each chunk looks something like this:
        //
        // | * |   e96277a570cd32432fjklfef
        // | |\ \
        // | | |/
        // | |/|

        // Parse out and remove the commit hashes.
        let mut chunks: Vec<(CommitHash, Vec<&str>)> = Vec::new();
        for mut raw_chunk in raw_chunks {
            // The commit hash should be the only alphanumeric sequence in
            // the chunk and it should be in the first line.
            let matches: Vec<_> = COMMIT_HASH_REGEX.find_iter(raw_chunk[0]).collect();
            if matches.len() != 1 {
                bail!(
                    "matched {} commit hashes in graph chunk:\n{:?}",
                    matches.len(),
                    raw_chunk
                );
            }
            let mattch = matches.first().unwrap();
            let hash = CommitHash::new(mattch.as_str());

            // We only want the graph bit, strip out the commit hash which we
            // only put in there as an anchor for this parsing.
            raw_chunk[0] = &raw_chunk[0][..mattch.range().start];

            chunks.push((hash, raw_chunk));
        }
        Ok(chunks)
    }
}

// Helper for OutputBuffer - a way to grab log info for a bunch of commits with
// a single git command. It's important that we don't do N git commands, that
// can really slow things down when the range is large.

struct CommitInfoBuffer {
    raw_buf: String, // Output straight from Git.
}

impl CommitInfoBuffer {
    // log_format must not contain %x00 as that's used internally for splitting
    // up the raw buffer.
    pub async fn new(
        repo: &Arc<impl Worktree>,
        range_spec: impl AsRef<OsStr>,
        log_format: &str,
    ) -> anyhow::Result<Self> {
        if log_format.contains("%x00") {
            bail!("NUL bytes not allowed in log format");
        }
        let raw_buf = repo
            .log(
                range_spec.as_ref(),
                format!("%H {log_format}%x00"),
                LogStyle::NoGraph,
            )
            .await?;
        // Hack: OsStr doesn't have a proper API, so just squash to utf-8, sorry
        // users.
        Ok(Self {
            raw_buf: String::from_utf8_lossy(&raw_buf).to_string(),
        })
    }

    pub fn info(&self) -> anyhow::Result<HashMap<CommitHash, &str>> {
        let b = self.raw_buf.trim();
        let b = b.strip_suffix('\0').unwrap_or(b);
        if b.is_empty() {
            return Ok(HashMap::new());
        }
        b.split('\0')
            .map(|raw_chunk| {
                let [hash, chunk] = raw_chunk
                    .splitn(2, ' ')
                    .collect::<Vec<_>>()
                    .try_into()
                    .map_err(|v: Vec<_>| {
                        anyhow::anyhow!("expected chunk split len 2, got {}", v.len())
                    })?;
                Ok((CommitHash::new(hash.trim()), chunk))
            })
            .collect()
    }
}

// Represents the buffer showing the current status of all the commits being tested.
struct OutputBuffer {
    // Pre-rendered lines containing static information (graph, commit log info etc).
    lines: Vec<String>,
    // lines[i] should be appended with the live status information of tests for status_commit[i].
    status_commits: HashMap<usize, CommitHash>,
}

impl OutputBuffer {
    pub fn empty() -> Self {
        Self {
            lines: Vec::new(),
            status_commits: HashMap::new(),
        }
    }

    pub async fn new<W: Worktree, S: AsRef<OsStr>>(
        repo: &Arc<W>,
        range_spec: S,
        log_format: &str,
    ) -> anyhow::Result<Self> {
        // All right this is gonna seem pretty hacky. We're gonna get the --graph log
        // as a text blob, then we're gonna use our pre-existing knowledge about
        // its contents as position anchors to patch it with the information we need.
        // This saves us having to actually write any algorithms ourselves. Basically
        // we only care about the structure of the DAG in so far as it influences the layout
        // of characters we're gonna display in the terminal. So, we just get
        // Git to tell us that exact information 🤷.
        // This is actually the same approach taken by the code I looked at in
        // the edamagit VSCode extension.
        // Note it's tricky because, even if you simplify it by fixing the
        // number of lines that the non-graph section of the output occupies,
        // the graph logic can still sometimes occupy more more lines when
        // history is very complex.
        //
        // So here's the idea: we just get git to dump out the graph, we divide
        // it into chunks that start with the '*' that identifies each commit.
        // Then we insert the extra information to the graph chunks.
        //
        // We want to display a) some more human-readable information about the
        // commit (i.e. what you get from logging with a more informative
        // --format) and b) our injected test status data. Overall this will
        // produce some other buffer. If it has less lines than the graph buffer
        // chunk, we can just append those lines onto the lines of the graph
        // buffer pairwise. If it has more lines then we will need to stretch
        // out the graph vertically to make space first.

        let (graph_buf, info_buf) = try_join(
            GraphBuffer::new(repo, range_spec.as_ref()),
            CommitInfoBuffer::new(repo, range_spec.as_ref(), log_format),
        )
        .await?;

        let commit_info = info_buf.info()?;
        let mut lines = Vec::new();
        let mut status_commits = HashMap::new();
        for (hash, mut chunk) in graph_buf.chunks()? {
            let log_info = commit_info
                .get(&hash)
                .with_context(|| format!("missing commit info for {:?}", hash))?;

            // We're gonna add our own newlines in so we don't need the one that
            // Git printed.
            let log_info = log_info.strip_suffix('\n').unwrap_or(log_info);
            let mut info_lines: Vec<&str> = log_info.split('\n').collect();

            // Here's where we'll inject the live status
            status_commits.insert(lines.len() + info_lines.len(), hash);
            info_lines.push("");

            let graph_line_deficit = info_lines.len() as isize - chunk.len() as isize;
            let extension_line;
            if graph_line_deficit > 0 {
                // We assume that the first line of the chunk will contain an
                // asterisk identifying the current commit, and some vertical
                // lines continuing up to the previous chunk. We just copy those
                // vertical lines and then add a new vertical lines pointing up
                // to the asterisk.
                //
                // I checked and it is in fact possible to have non-vertical
                // lines on the same line as the asterisk. E.g. check the linux
                // kernel history, search back to commit 578cc98b66f5a5 and you
                // will see it. So we need to replace diagnoals with verticals
                // too.
                extension_line = GRAPH_COMPONENT_REGEX.replace_all(chunk[0], "|");
                for _ in 0..graph_line_deficit {
                    chunk.insert(1, &extension_line);
                }
            } else {
                // Append empty entries to the info lines so that the zip below works nicely.
                info_lines.append(&mut vec![""; -graph_line_deficit as usize]);
            }
            assert_eq!(info_lines.len(), chunk.len());

            lines.append(
                &mut chunk
                    .iter()
                    .zip(info_lines.iter())
                    .map(|(graph, info)| (*graph).to_owned() + *info)
                    // TODO: can we get rid of the collect and just call .join on the map iterator?
                    .collect::<Vec<_>>(),
            );
        }
        Ok(Self {
            lines,
            status_commits,
        })
    }

    fn render<'a>(
        &'a self,
        statuses: &'a HashMap<CommitHash, HashMap<TestName, TrackedTestCase>>,
        result_url_base: &str,
    ) -> Text<'a> {
        if self.lines.is_empty() {
            return "[range empty]".into();
        }
        self.lines
            .iter()
            .enumerate()
            .map(|(i, log_line)| -> Line {
                let mut spans = vec![Span::from(log_line)];
                if let Some(hash) = self.status_commits.get(&i) {
                    if let Some(tracked_cases) = statuses.get(hash) {
                        spans.extend(self.render_cases(tracked_cases.values(), result_url_base));
                    }
                }
                Line::from_iter(spans)
            })
            .collect::<Text>()
    }

    fn render_case<'a>(
        test_case: &'a TestCase,
        status: &'a TestStatus,
        result_url_base: &str,
    ) -> Vec<Span<'a>> {
        let status_part = match status {
            TestStatus::Enqueued => Span::new("⏳"),
            TestStatus::Started => Span::new("🔄"),
            TestStatus::Finished(Ok(result)) => {
                if result.exit_code == 0 {
                    Span::new("✅")
                } else {
                    Span::new("❌")
                }
            }
            TestStatus::Finished(Err(TestInconclusive::Canceled)) => Span::new("⛔"),
            TestStatus::Finished(Err(_)) => Span::new("🔥"),
        }
        .with_url(format!(
            "{}/{}/{}",
            result_url_base,
            Database::result_relpath(test_case).to_string_lossy(),
            if test_case.test.separate_outputs {
                "stdout.txt"
            } else {
                "output.txt"
            }
        ));
        vec![
            Span::new(test_case.test.name.to_string()).with_class(Class::TestName),
            Span::new(": "),
            status_part,
            Span::new(" "),
        ]
    }

    fn render_cases<'a>(
        &self,
        tracked_cases: impl IntoIterator<Item = &'a TrackedTestCase>,
        result_url_base: &str,
    ) -> Vec<Span<'a>> {
        let mut tracked_cases: Vec<_> = tracked_cases.into_iter().collect();
        tracked_cases.sort_by_key(|tc| &tc.test_case.test.name);
        let mut spans = Vec::new();
        for tracked_case in tracked_cases {
            spans.extend(Self::render_case(
                &tracked_case.test_case,
                &tracked_case.status,
                result_url_base,
            ));
        }
        spans
    }
}

#[cfg(test)]
mod tests {
    use core::str;
    use std::sync::Arc;

    use googletest::{expect_that, prelude::eq};

    use crate::{
        git::{
            test_utils::{TempRepo, WorktreeExt},
            Commit,
        },
        test::{test_utils::TestBuilder, CachePolicy, ExitCode, Test, TestResult},
    };

    use super::*;

    fn fake_test(name: &str, cache_policy: CachePolicy) -> Arc<Test> {
        Arc::new(
            TestBuilder::new(name, "", [""])
                .cache_policy(cache_policy)
                .build(),
        )
    }

    fn fake_notif(commit_hash: &CommitHash, test: &Arc<Test>, status: TestStatus) -> Notification {
        Notification {
            test_case: TestCase {
                commit_hash: commit_hash.clone(),
                cache_hash: Some(commit_hash.clone().into()),
                test: test.clone(),
            },
            status,
        }
    }

    // Abbreviate a commit message.
    fn abbrev(commit: &Commit) -> &str {
        // TODO: The degree of abbreviation here probably depends on git
        // configuration. This will probably fail on someone's machine.
        &<CommitHash as AsRef<str>>::as_ref(&commit.hash)[..7]
    }

    async fn fake_completion(exit_code: ExitCode) -> TestStatus {
        TestStatus::Finished(Ok(TestResult { exit_code }))
    }

    #[googletest::test]
    #[tokio::test]
    async fn output_buffer_smoke() {
        let repo = Arc::new(TempRepo::new().await.unwrap());
        repo.commit("1").await.unwrap();
        let commit2 = repo.commit("2").await.unwrap();
        let commit3 = repo.commit("3").await.unwrap();
        let test1 = fake_test("my_test1", CachePolicy::ByCommit);
        let test2 = fake_test("my_test2", CachePolicy::ByCommit);

        let ob = OutputBuffer::new(&repo, format!("{}^..HEAD", commit2.hash), "%h %s")
            .await
            .expect("failed to build OutputBuffer");
        let mut tracked_cases = HashMap::new();
        for notif in [
            fake_notif(&commit3.hash, &test1, TestStatus::Enqueued),
            fake_notif(&commit3.hash, &test2, fake_completion(0).await),
            fake_notif(
                &commit2.hash,
                &test1,
                TestStatus::Finished(Err(TestInconclusive::Error("oh no".to_owned()))),
            ),
            fake_notif(&commit2.hash, &test2, TestStatus::Started),
        ] {
            update_tracked_cases(&mut tracked_cases, Arc::new(notif));
        }

        let buf = format!("{}", ob.render(&tracked_cases, "myhost").ansi());
        expect_that!(
            // The colored crate does not have any useful way to disable it from
            // this test code, only globally. This clashes with parallel testing.
            // At first I thought about just having tests take a global lock but
            // then realied that if one test failed, the other would panic
            // holding the lock. Also parallelism is nice. So, we just ignore
            // the color.
            *strip_ansi_escapes::strip_str(str::from_utf8(buf.as_bytes()).unwrap()),
            eq(format!(
                "* {commit3} 3\n\
                | my_test1: ⏳ my_test2: ✅ \n\
                * {commit2} 2\n\
                | my_test1: 🔥 my_test2: 🔄 \n",
                commit3 = abbrev(&commit3),
                commit2 = abbrev(&commit2)
            ))
        );
    }

    #[googletest::test]
    #[tokio::test]
    async fn output_buffer_octopus() {
        let repo = Arc::new(TempRepo::new().await.unwrap());
        let base_commit = repo.commit("base").await.unwrap();
        let join_commit = repo.commit("join").await.unwrap();
        let commit1 = repo.commit("1").await.unwrap();
        repo.checkout(&base_commit.hash).await.unwrap();
        let commit2 = repo.commit("2").await.unwrap();
        repo.checkout(&base_commit.hash).await.unwrap();
        let commit3 = repo.commit("3").await.unwrap();
        let merge = repo
            .merge(&[
                commit1.hash.clone(),
                commit2.hash.clone(),
                commit3.hash.clone(),
            ])
            .await
            .unwrap();
        let test1 = fake_test("my_test1", CachePolicy::ByCommit);
        let test2 = fake_test("my_test2", CachePolicy::ByCommit);

        let ob = OutputBuffer::new(&repo, format!("{}..HEAD", base_commit.hash), "%h %s")
            .await
            .expect("failed to build OutputBuffer");

        let mut tracked_cases = HashMap::new();
        for notif in [
            fake_notif(&commit3.hash, &test1, TestStatus::Enqueued),
            fake_notif(&commit3.hash, &test2, fake_completion(0).await),
            fake_notif(
                &commit2.hash,
                &test1,
                TestStatus::Finished(Err(TestInconclusive::Error("oh no".to_owned()))),
            ),
            fake_notif(&commit2.hash, &test2, TestStatus::Started),
        ] {
            update_tracked_cases(&mut tracked_cases, Arc::new(notif));
        }

        let buf = format!("{}", ob.render(&tracked_cases, "myhost").ansi());

        // Note this is a kinda weird log. We excluded the common ancestor of all the commits.
        // Also note it's a kinda weird input because we haven't provided any
        // statuses all of the commits (this does momentarily happen IRL).
        expect_that!(
            *strip_ansi_escapes::strip_str(str::from_utf8(buf.as_bytes()).unwrap()),
            eq(format!(
                "*-.   {merge} merge commit\n\
                |\\ \\  \n\
                | | | \n\
                | | * {commit2} 2\n\
                | |   my_test1: 🔥 my_test2: 🔄 \n\
                | * {commit1} 1\n\
                | | \n\
                | * {join} join\n\
                |   \n\
                * {commit3} 3\n\
                | my_test1: ⏳ my_test2: ✅ \n",
                merge = abbrev(&merge),
                commit3 = abbrev(&commit3),
                commit2 = abbrev(&commit2),
                commit1 = abbrev(&commit1),
                join = abbrev(&join_commit)
            ))
        );
    }

    #[googletest::test]
    #[tokio::test]
    async fn output_buffer_empty() {
        let repo = Arc::new(TempRepo::new().await.unwrap());
        let base_commit = repo.commit("base").await.unwrap();
        repo.commit("join").await.unwrap();
        let commit1 = repo.commit("1").await.unwrap();
        repo.checkout(&base_commit.hash).await.unwrap();
        let commit2 = repo.commit("2").await.unwrap();
        repo.checkout(&base_commit.hash).await.unwrap();
        let commit3 = repo.commit("3").await.unwrap();
        repo.merge(&[commit1.hash, commit2.hash.clone(), commit3.hash.clone()])
            .await
            .unwrap();
        let test1 = fake_test("my_test1", CachePolicy::ByCommit);
        let test2 = fake_test("my_test2", CachePolicy::ByCommit);

        let ob = OutputBuffer::new(&repo, format!("{0}..{0}", base_commit.hash), "%h %s")
            .await
            .expect("failed to build OutputBuffer");
        let mut tracked_cases = HashMap::new();
        for notif in [
            fake_notif(&commit3.hash, &test1, TestStatus::Enqueued),
            fake_notif(&commit3.hash, &test1, fake_completion(0).await),
            fake_notif(
                &commit2.hash,
                &test2,
                TestStatus::Finished(Err(TestInconclusive::Error("oh no".to_owned()))),
            ),
            fake_notif(&commit2.hash, &test2, TestStatus::Started),
        ] {
            update_tracked_cases(&mut tracked_cases, Arc::new(notif));
        }

        let buf = format!("{}", ob.render(&tracked_cases, "myhost").ansi());
        expect_that!(
            *strip_ansi_escapes::strip_str(str::from_utf8(buf.as_bytes()).unwrap()),
            eq("[range empty]\n".to_owned())
        );
    }
}
