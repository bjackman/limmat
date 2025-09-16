use std::{
    fs::{create_dir, create_dir_all, File, OpenOptions},
    io::ErrorKind::AlreadyExists,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{bail, Context, Result};
#[allow(unused_imports)]
use log::debug;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use tempfile::NamedTempFile;

use crate::{
    flock::{ExclusiveFlock, SharedFlock},
    git::Hash,
    test::{ConfigHash, ExitCode, TestCase, TestName, TestResult},
    util::IoResultExt as _,
};

// Result database similar to the design described in
// https://github.com/bjackman/git-brisect?tab=readme-ov-file#the-result-directory
// TODO: Actually we should probably separate it by the repo lol. But how?
pub struct Database {
    pub base_dir: PathBuf,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
struct TestResultEntry {
    config_hash: ConfigHash,
    result: TestResult,
}

pub enum LookupResult {
    // Result found in the the database, here it is.
    FoundResult(DatabaseEntry),
    // No result, you can run it and write the output here.
    YouRunIt(DatabaseOutput),
}

// "Database" which is really just a directory. Entries are flocked via their main JSON file.
// I am not really sure if this flocking is safe if you open the same entry
// twice within the same process:
// https://stackoverflow.com/questions/79266574/is-flock-per-ofd-or-per-process-per-file
// For now I am just gonna assume flock has the most helpful semantics among the
// range of ambiguity and hope it's fine.
impl Database {
    pub fn create_or_open(base_dir: &Path) -> anyhow::Result<Self> {
        create_dir_all(base_dir).context(format!(
            "creating result database dir at {}",
            base_dir.display()
        ))?;
        Ok(Self {
            base_dir: base_dir.to_owned(),
        })
    }

    pub fn result_relpath(test_case: &TestCase) -> PathBuf {
        Path::new(test_case.storage_hash()).join(&test_case.test.name)
    }

    fn result_path(&self, hash: &Hash, test_name: &TestName) -> PathBuf {
        self.base_dir.join::<&str>(hash.as_ref()).join(test_name)
    }

    // Either get or create a result in the database. If there's a test running,
    // this blocks until it's done.
    pub async fn lookup(&self, test_case: &TestCase) -> Result<LookupResult> {
        let result_dir = self.result_path(test_case.storage_hash(), &test_case.test.name);
        create_dir_all(&result_dir)
            .with_context(|| format!("creating commit result dir at {}", result_dir.display()))?;
        let json_path = result_dir.join("result.json");

        let parse_result = |json: &str| -> Option<TestResultEntry> {
            // Manually ignore empty JSON to avoid log spam.
            if json.is_empty() {
                return None;
            }
            match serde_json::from_str::<TestResultEntry>(json) {
                Ok(test_result) => {
                    // Has the configuration changed? if not we need to rerun regardless.
                    if test_result.config_hash == test_case.test.config_hash {
                        // Was the test configured to accept cached results?
                        if test_case.cache_hash.is_some() {
                            // Cool, we're done.
                            return Some(test_result);
                        }
                    }
                }
                Err(e) => {
                    // This probably just means limmat got killed before we finished
                    // writing the result.
                    debug!(
                        "Error reading result JSON from {}: {e} - JSON\n{:?}",
                        json_path.display(),
                        json,
                    );
                }
            }
            None
        };

        // Don't block forever.
        for _ in 0..5 {
            let json_file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(result_dir.join("result.json"))
                .context("opening result JSON")?;
            let flock = SharedFlock::new(json_file)
                .await
                .context("locking JSON file for reading")?;

            if let Some(test_result) = parse_result(flock.content()) {
                return Ok(LookupResult::FoundResult(DatabaseEntry {
                    base_path: result_dir.clone(),
                    result: test_result,
                    _json_flock: flock,
                    #[cfg(test)]
                    _tempfile: None,
                }));
            }

            // Seems we have to run the test. For that we'll need an exclusive lock.
            let flock = flock.upgrade().await.context("upgrading JSON file lock")?;

            // But, that upgrade wasn't atomic, someone else might have jumped
            // in and run the test. Check if that's the case...
            if parse_result(flock.content()).is_some() {
                // OK great someone ran the test, so we just wanna return the result. But for that
                // we need to downgrade the lock to a shared lock, which is also not atomic. At the
                // time of writing, this is harmless: we know the test case is cacheable (otherwise
                // parse_result never returns Some) so if someone else gets the lock during the
                // downgrade, they aren't gonna re-run the test. But, we want the flexibility to
                // later implement at-will re-runs of tests, and more importantly deletion of test
                // results. So we downgrade the lock by just going back around this loop.
                continue;
            }

            return Ok(LookupResult::YouRunIt(
                DatabaseOutput::new(
                    result_dir,
                    test_case.test.config_hash.clone(),
                    flock,
                    test_case.test.separate_outputs,
                )
                .context("creating database entry")?,
            ));
        }
        bail!("too much database contention, something fishy going on")
    }
}

// Existing entry in the database. Until you drop this object, the entry is read-locked, meaning
// you prevent anyone else from re-running the test.
#[derive(Debug)]
pub struct DatabaseEntry {
    base_path: PathBuf,
    result: TestResultEntry,
    _json_flock: SharedFlock,
    #[cfg(test)]
    _tempfile: Option<NamedTempFile>,
}

impl DatabaseEntry {
    pub fn result(&self) -> &TestResult {
        &self.result.result
    }

    pub fn exit_code(&self) -> ExitCode {
        self.result.result.exit_code
    }

    pub fn stdout_path(&self) -> PathBuf {
        self.base_path.join("stdout.txt")
    }

    pub fn stderr_path(&self) -> PathBuf {
        self.base_path.join("stderr.txt")
    }

    pub fn artifacts_dir(&self) -> PathBuf {
        self.base_path.join("artifacts")
    }
}

// Output for an individual test job, which may or may not be stored into the
// database depending on where it came from. If it is, it ncludes an exclusive
// lock on the database entry, nobody can read the result or run the test case
// until you drop this object.
pub struct DatabaseOutput {
    base_dir: PathBuf,      // Must exist.
    artifacts_dir: PathBuf, // This too.
    // TODO: this is a mess, probably instead we should use a trait object of some kind. This was
    // done this way in part to avoid polluting the code with a trait object but
    // maybe it can be done cleanly specifically within the database module.
    stdout_opened: bool,
    provided_stdout: Option<Stdio>,
    stderr_opened: bool,
    provided_stderr: Option<Stdio>,
    status_written: bool,
    config_hash: ConfigHash,
    json_flock: ExclusiveFlock,
    separate_outputs: bool,
    // For merged output mode, we need to share the same file between stdout and stderr
    shared_output_file: Option<File>,
}

impl DatabaseOutput {
    fn new(
        base_dir: PathBuf, // Must exist.
        config_hash: ConfigHash,
        json_flock: ExclusiveFlock,
        separate_outputs: bool,
    ) -> anyhow::Result<Self> {
        debug!("Creating database entry at {base_dir:?}");
        let artifacts_dir = base_dir.join("artifacts").to_owned();
        create_dir(&artifacts_dir)
            .ignore(AlreadyExists)
            .context("creating artifacts dir")?;
        Ok(Self {
            artifacts_dir,
            base_dir,
            stdout_opened: false,
            provided_stdout: None,
            stderr_opened: false,
            provided_stderr: None,
            status_written: false,
            config_hash,
            json_flock,
            separate_outputs,
            shared_output_file: None,
        })
    }

    // Create a "DatabaseOutput" that is not actually in the database, this can be used for
    // storing "ephemeral" results (not in the sense that we destroy them
    // ourselves, just in the sense that we don't really look after them and the
    // user is likely to delete them later). base_dir must exist.
    pub async fn ephemeral(
        base_dir: PathBuf,
        stdout: Stdio,
        stderr: Stdio,
        separate_outputs: bool,
    ) -> anyhow::Result<Self> {
        let artifacts_dir = base_dir.join("artifacts").to_owned();
        create_dir(&artifacts_dir).context("creating artifacts dir")?;
        let json_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(base_dir.join("result.json"))
            .context("creating ephemeral result JSON")?;
        Ok(Self {
            base_dir,
            artifacts_dir,
            stdout_opened: false,
            provided_stdout: Some(stdout),
            stderr_opened: false,
            provided_stderr: Some(stderr),
            status_written: false,
            config_hash: "".to_string(),
            // Note the locking is unnecessary in the ephemeral case but it's
            // just easier to do it anyway.
            json_flock: ExclusiveFlock::new(json_file)
                .await
                .context("locking ephemeral JSON result")?,
            separate_outputs,
            shared_output_file: None,
        })
    }

    fn stdout_file(&mut self) -> anyhow::Result<File> {
        let path = self.base_dir.join("stdout.txt");
        File::create(&path).with_context(|| format!("creating {}", path.display()))
    }

    fn stderr_file(&mut self) -> anyhow::Result<File> {
        let path = self.base_dir.join("stderr.txt");
        File::create(&path).with_context(|| format!("creating {}", path.display()))
    }

    fn output_file(&mut self) -> anyhow::Result<File> {
        let path = self.base_dir.join("output.txt");
        File::create(&path).with_context(|| format!("creating {}", path.display()))
    }

    pub fn stdout(&mut self) -> Result<Stdio> {
        assert!(!self.stdout_opened);
        self.stdout_opened = true;
        if let Some(stdout) = self.provided_stdout.take() {
            return Ok(stdout);
        }

        if self.separate_outputs {
            Ok(self.stdout_file()?.into())
        } else {
            // Merged output mode - create shared file if not already created
            if self.shared_output_file.is_none() {
                self.shared_output_file = Some(self.output_file()?);
            }
            Ok(self
                .shared_output_file
                .as_ref()
                .unwrap()
                .try_clone()?
                .into())
        }
    }

    pub fn stderr(&mut self) -> Result<Stdio> {
        assert!(!self.stderr_opened);
        self.stderr_opened = true;
        if let Some(stderr) = self.provided_stderr.take() {
            return Ok(stderr);
        }

        if self.separate_outputs {
            Ok(self.stderr_file()?.into())
        } else {
            // Merged output mode - create shared file if not already created
            if self.shared_output_file.is_none() {
                self.shared_output_file = Some(self.output_file()?);
            }
            Ok(self
                .shared_output_file
                .as_ref()
                .unwrap()
                .try_clone()?
                .into())
        }
    }

    // Set the result and return the created entry. Unfortunately because flock
    // downgrades are non-atomic, it's possible for this to fail as someone can
    // grab the lock while we are downgrading and delete the entry. (At time of
    // writing there is no logic to delete entries but I plan to implement this).
    // I'm hopeful that this won't really happen in practice, hopefully flock
    // implementations treat shared locks as having higher-priority, so the
    // downgrade rarely gets "beaten" by an exclusive lock. (Also, it should be
    // rare that we delete an entry when there's a test running that depends on
    // it).
    pub async fn set_result(mut self, result: &TestResult) -> anyhow::Result<DatabaseEntry> {
        assert!(!self.status_written);
        self.status_written = true;
        let entry = TestResultEntry {
            config_hash: self.config_hash.clone(),
            result: result.clone(),
        };
        self.json_flock
            .set_content(&serde_json::to_vec(&entry).expect("failed to serialize TestStatus"))
            .context("writing JSON result")?;
        Ok(DatabaseEntry {
            base_path: self.base_dir,
            result: TestResultEntry {
                config_hash: self.config_hash,
                result: result.clone(),
            },
            _json_flock: self
                .json_flock
                .downgrade()
                .await
                .context("downgrading result JSON flock")?,
            #[cfg(test)]
            _tempfile: None,
        })
    }

    pub fn artifacts_dir(&mut self) -> &Path {
        &self.artifacts_dir
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write as _, sync::Arc};

    use tempfile::TempDir;

    use crate::{git::Commit, test::test_utils::TestBuilder};

    use super::*;

    #[tokio::test]
    async fn test_corrupted_result() {
        let db_dir = TempDir::new().unwrap();
        let db = Database::create_or_open(db_dir.path()).unwrap();

        // Setup: Create a corrupted database entry. This simulates Limmat
        // getting killed in the middle of writing.
        // Best way to make sure we are corrupting data that the database will
        // really is to have the database write it in the first place.
        let test_case = TestCase::new(
            Commit::arbitrary(),
            Arc::new(TestBuilder::new("my_test", "", [""]).build()),
        );
        let json_path = {
            let mut output = match db.lookup(&test_case).await.unwrap() {
                LookupResult::FoundResult(_) => panic!("Found result in empty database"),
                LookupResult::YouRunIt(output) => output,
            };
            output
                .stderr_file()
                .unwrap()
                .write_all(b"hello stderr\n")
                .unwrap();
            output
                .stdout_file()
                .unwrap()
                .write_all(b"hello stdout\n")
                .unwrap();
            let json_path = output.base_dir.join("result.json");
            output
                .set_result(&TestResult { exit_code: 1 })
                .await
                .unwrap();
            json_path
        };
        {
            let mut f = OpenOptions::new()
                .write(true)
                .open(json_path)
                .expect("couldn't open JSON file");
            for _ in 0..16 {
                f.write_all(b"I DON`T TIHKN THA'TS JASON BATMAN,,,\n")
                    .unwrap();
            }
        }

        // Act: Now open the entry. It should just silently act as though nothing was there.
        {
            let output = match db.lookup(&test_case).await.unwrap() {
                LookupResult::FoundResult(e) => panic!("successfully read corrupted JSON? {e:?}"),
                LookupResult::YouRunIt(output) => output,
            };
            output
                .set_result(&TestResult { exit_code: 2 })
                .await
                .unwrap();
        }
        // Now it should be valid again.
        match db.lookup(&test_case).await.unwrap() {
            LookupResult::FoundResult(entry) => assert_eq!(entry.result.result.exit_code, 2),
            LookupResult::YouRunIt(_) => panic!("no JSON found after DB corruption"),
        };
    }
}
// TODO:
// - Test behaviour on already-existing directories
