use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
    ffi::OsString,
    hash::Hash as _,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context as _};
#[allow(unused_imports)]
use log::debug;
use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use sha3::{Digest, Sha3_256};

use crate::{
    dag::{Dag, GraphNode},
    resource::{self, Pools, ResourceKey},
    test::{self, CachePolicy, ExitCode, TestDag, TestName},
    util::DigestHasher,
};

#[derive(Deserialize, JsonSchema, Debug, Hash, Clone)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub enum Resource {
    /// Shorthand for describing a singular resource, equivalent to setting count=1.
    Bare(String),
    /// Specify resources where you don't care about the value of the token.
    Counted { name: String, count: usize },
    /// Specify resources with explicitly set token values. These will be passed
    /// into the job environment via LIMMAT_RESOURCE_<name>_<n> where n is 0-indexed.
    // TODO: If there's only one, we should also export it without the _<n>
    Explicit { name: String, tokens: Vec<String> },
}

impl Resource {
    pub fn name(&self) -> &str {
        match self {
            Self::Bare(n) => n,
            Self::Counted { name: n, count: _ } => n,
            Self::Explicit { name: n, tokens: _ } => n,
        }
    }

    pub fn count(&self) -> usize {
        match self {
            Self::Bare(_) => 1,
            Self::Counted { name: _, count: c } => *c,
            Self::Explicit { name: _, tokens: t } => t.len(),
        }
    }
}

#[derive(Deserialize, JsonSchema, Debug, Hash, Clone)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub enum Command {
    Shell(String),
    Raw(Vec<String>),
}

impl Command {
    pub fn program(&self) -> OsString {
        match self {
            Self::Shell(_) => "bash".into(), // TODO: Figure out the user's configured shell.
            Self::Raw(args) => args[0].clone().into(),
        }
    }

    pub fn args(&self) -> Vec<OsString> {
        match self {
            Self::Shell(cmd) => vec!["-c".into(), cmd.into()],
            Self::Raw(args) => args[1..].iter().map(|s| s.into()).collect(),
        }
    }
}

#[derive(Deserialize, JsonSchema, Debug, Hash, Clone)]
#[serde(deny_unknown_fields)]
pub struct Test {
    name: String,
    command: Command,
    #[serde(default = "default_requires_worktree")]
    requires_worktree: bool,
    #[serde(default = "default_true")]
    /// If this is disabled, the test is only run when explicitly requested in
    /// the command-line via the --tests arg.
    run_by_default: bool,
    // TODO: This should only refer to resource names.
    resources: Option<Vec<Resource>>,
    #[serde(default = "default_shutdown_grace_period")]
    /// When a job is no longer needed it's SIGTERMed. If it doesn't respond (by
    /// dying) after this duration it will then be SIGKILLed. This also affects
    /// the overall shutdown of limmat so do not set this to longer than you are
    /// willing to wait when you terminate this program.
    shutdown_grace_period_s: u64,
    #[serde(default = "default_cache_policy")]
    cache: CachePolicy,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    /// If the command exits with an error code listed in this field, instead of
    /// being considered a "failure", it's considered an "error". Errors are not
    /// cached - the erroring test will be re-run when Limmat restarts. You can
    /// use this to report environmental failures such as dependencies missing
    /// fom the host system. 0 is not allowed.
    error_exit_codes: Vec<ExitCode>,
    #[serde(default = "default_separate_outputs")]
    /// When false (default), stdout and stderr are merged into output.txt.
    /// When true, they are kept separate as stdout.txt and stderr.txt.
    separate_outputs: bool,
}

fn default_requires_worktree() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_separate_outputs() -> bool {
    false
}

// This implementation is only valid for Tests among those registered for a single Manager.
impl GraphNode for Test {
    type NodeId = String;

    fn id(&self) -> impl Borrow<String> {
        &self.name
    }

    fn child_ids(&self) -> Vec<impl Borrow<String>> {
        self.depends_on.iter().collect()
    }
}

impl Test {
    // Convert to the "real" object. other_tests is the set of other tests that
    // have already been parsed, which must include all of these test's
    // transitive dependencies (or this will panic).
    pub fn parse(&self, other_tests: &Dag<Arc<test::Test>>) -> anyhow::Result<test::Test> {
        let mut seen_resources = HashSet::new();
        for resource in self.resources.as_ref().unwrap_or(&vec![]) {
            if seen_resources.contains(&resource.name()) {
                // TODO: Need better error messages.
                bail!("duplicate resource reference {:?}", resource.name());
            }
            seen_resources.insert(resource.name());
        }
        let mut needs_resources: HashMap<ResourceKey, usize> = self
            .resources
            .as_ref()
            .unwrap_or(&vec![])
            .iter()
            .map(|r| (ResourceKey::UserToken(r.name().to_owned()), r.count()))
            .collect();
        if self.requires_worktree {
            needs_resources.insert(ResourceKey::Worktree, 1);
        }

        // Hash the config, also taking into account the hashes of the
        // dependency test configs.
        let mut hasher = DigestHasher {
            digest: Sha3_256::new(),
        };
        self.hash(&mut hasher);
        for dep_name in &self.depends_on {
            other_tests
                .node(&TestName::new(dep_name))
                .unwrap()
                .config_hash
                .hash(&mut hasher);
        }
        let config_hash = hex::encode(hasher.digest.finalize());
        debug!("Config hash for {}: {:?}", self.name, config_hash);

        let error_exit_codes: HashSet<_> = self.error_exit_codes.iter().cloned().collect();
        if error_exit_codes.contains(&0) {
            bail!("error_exit_codes must not contain 0");
        }

        Ok(test::Test {
            name: TestName::new(self.name.clone()),
            program: self.command.program(),
            args: self.command.args(),
            needs_resources,
            shutdown_grace_period: Duration::from_secs(self.shutdown_grace_period_s),
            cache_policy: self.cache,
            config_hash,
            depends_on: self.depends_on.iter().map(TestName::new).collect(),
            error_exit_codes,
            separate_outputs: self.separate_outputs,
        })
    }
}

fn default_cache_policy() -> CachePolicy {
    // Hard to choose a default here. Rationale for this choice: It's weird not
    // to want any caching at all. Almost all of the time you want ByTree, but
    // ByCommit will give you 80% of the value, and lots of people don't think
    // about the difference between tree and commit anyway.
    CachePolicy::ByCommit
}

fn default_shutdown_grace_period() -> u64 {
    60
}

#[derive(Deserialize, JsonSchema, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_num_worktrees")]
    pub num_worktrees: usize,
    resources: Option<Vec<Resource>>,
    // Default is just here to make testing snippets from the documentation easier.
    #[serde(default)]
    tests: Vec<Test>,
}

fn default_num_worktrees() -> usize {
    8
}

type ResourceTokens = HashMap<ResourceKey, Vec<String>>;

impl Config {
    fn parse_resource_tokens(&self) -> ResourceTokens {
        self.resources
            .as_ref()
            .unwrap_or(&vec![])
            .iter()
            .map(|resource| {
                (
                    ResourceKey::UserToken(resource.name().to_owned()),
                    match resource {
                        Resource::Explicit { name: _, tokens } => tokens.clone(),
                        _ => (0..resource.count())
                            .map(|i| format!("{}-{}", resource.name(), i))
                            .collect(),
                    },
                )
            })
            .collect()
    }

    fn parse_tests<S: AsRef<str>>(
        &self,
        resource_tokens: &ResourceTokens,
        skip_tests: impl IntoIterator<Item = S>,
        only_tests: impl IntoIterator<Item = S>,
    ) -> anyhow::Result<Dag<Arc<test::Test>>> {
        let skip_tests: Vec<Regex> = skip_tests
            .into_iter()
            .map(|s| Regex::new(s.as_ref()))
            .collect::<Result<Vec<_>, _>>()
            .context("compiling skip_tests regexes")?;

        let only_tests: Vec<Regex> = only_tests
            .into_iter()
            .map(|s| Regex::new(s.as_ref()))
            .collect::<Result<Vec<_>, _>>()
            .context("compiling tests filter regexes")?;

        let tests = Dag::new(
            self.tests
                .iter()
                .filter(|t| {
                    if !only_tests.is_empty() {
                        if !only_tests.iter().any(|r| r.is_match(&t.name)) {
                            return false;
                        }
                    } else if !t.run_by_default {
                        return false;
                    }
                    !skip_tests.iter().any(|r| r.is_match(&t.name))
                })
                .cloned(),
        )
        .context("parsing test dependency graph")?;
        // This is beginning to be kinda cool, we can map between DAGs of
        // different types of objects.  It's still kinda awkward that users of
        // this fold mechanism have to manually insert their new nodes into the
        // accumulator, this also means there are two unwrap calls - once when
        // adding the new node and once when referring to existing nodes.
        // I suspect it's possible to make an even cooler API that knows that we
        // are mapping between two isomorphic graphs and so these "dereferences" can't fail.
        let tests = tests
            .bottom_up()
            .try_fold(
                Dag::empty(),
                |parsed_dag, test_conf: &Test| -> anyhow::Result<Dag<Arc<test::Test>>> {
                    let new_node = Arc::new(test_conf.parse(&parsed_dag)?);
                    Ok(parsed_dag.with_node(new_node).unwrap())
                },
            )
            .context("parsing tests")?;

        // Check for invalid resource references.
        for test in tests.nodes() {
            for key in test.needs_resources.keys() {
                if let ResourceKey::UserToken(name) = key {
                    if !resource_tokens.contains_key(key) {
                        bail!(
                            "undefined resource {:?} referenced in test {:?}",
                            name,
                            test.name
                        );
                    }
                }
            }
        }

        Ok(tests)
    }
}

// Messy type to try and capture a pretty arbitrary aspect of initialising the
// pre-requisites to run jobs.
// Construct via from. This does NOT create worktrees, that's why it has a
// num_worktrees field to tell you how many you'll need to create and insert
// into the pools.
// The reason for this is that for some reason I decided that the num_worktrees
// option should be ignored when running one-shot tests. This was dumb and made
// things unnecessarily complicated.
#[derive(Debug)]
pub struct ParsedConfig {
    pub source_path: PathBuf,
    pub num_worktrees: usize,
    pub resource_pools: Arc<Pools>,
    pub tests: TestDag,
}

impl ParsedConfig {
    pub fn new<S: AsRef<str>>(
        config: Config,
        source_path: impl Into<PathBuf>,
        skip_tests: impl IntoIterator<Item = S>,
        only_tests: impl IntoIterator<Item = S>,
    ) -> anyhow::Result<Self> {
        let resource_tokens = config.parse_resource_tokens();
        let tests = config.parse_tests(&resource_tokens, skip_tests, only_tests)?;
        let resources: HashMap<ResourceKey, Vec<resource::Resource>> = resource_tokens
            .into_iter()
            .map(|(key, tokens)| {
                (
                    key,
                    tokens
                        .into_iter()
                        .map(resource::Resource::UserToken)
                        .collect(),
                )
            })
            .collect();
        Ok(Self {
            num_worktrees: config.num_worktrees,
            resource_pools: Arc::new(Pools::new(resources)),
            source_path: source_path.into(),
            tests,
        })
    }
}

#[cfg(test)]
mod tests {
    use googletest::{assert_that, expect_that, prelude::*};
    use pretty_assertions::assert_eq;
    use regex::Regex;
    use schemars::schema_for;

    use super::*;

    // Poor man's replacement for google3's "generated files" feature: just check
    // the generated file in and have a test to check it's not out of date.
    #[googletest::test]
    fn test_json_schema_updated() {
        let got = include_str!("../limmat.schema.json");
        let want = serde_json::to_string_pretty(&schema_for!(Config)).unwrap();
        assert_eq!(
            got, want,
            "Config json-schema seems to have changed. Want 'right' got 'left'"
        );
    }

    // Check all the config snippts in the README can at least be parsed.
    #[googletest::test]
    fn test_readme_snippets() {
        let code_block_regex = Regex::new(r"(?m)```(\w+?)\n((.|\n)+?)```").unwrap();
        let toml_blocks = code_block_regex
            .captures_iter(include_str!("../README.md"))
            .filter_map(|captures| {
                let lang = captures.get(1).expect("nothing in capture group 0");
                if lang.as_str() != "toml" {
                    debug!("{}", lang.as_str());
                    None
                } else {
                    Some(
                        captures
                            .get(2)
                            .expect("nothing in capture group 1")
                            .as_str(),
                    )
                }
            })
            .collect::<Vec<&str>>();
        assert_that!(
            toml_blocks,
            not(empty()),
            "No TOML found in README - test bug?"
        );
        for toml in toml_blocks {
            expect_that!(
                toml::from_str(toml).map(|config| ParsedConfig::new(config, "/fake/path", Vec::<String>::new(), Vec::<String>::new())),
                ok(anything())
            );
        }
    }

    #[googletest::test]
    fn test_default_filtering() {
        let config_toml = r#"
            [[tests]]
            name = "default_test"
            command = ["echo", "hello"]

            [[tests]]
            name = "non_default_test"
            command = ["echo", "hello"]
            run_by_default = false
        "#;
        let config: Config = toml::from_str(config_toml).unwrap();

        // Case 1: No filters. Should only include default_test.
        let parsed = ParsedConfig::new(config.clone(), "/fake", Vec::<&str>::new(), Vec::<&str>::new()).unwrap();
        assert_that!(parsed.tests.node(&TestName::new("default_test")), some(anything()));
        assert_that!(parsed.tests.node(&TestName::new("non_default_test")), none());

        // Case 2: Explicit filter for non_default. Should include it.
        let parsed = ParsedConfig::new(config, "/fake", Vec::<&str>::new(), vec!["non_default_test"]).unwrap();
        assert_that!(parsed.tests.node(&TestName::new("non_default_test")), some(anything()));
    }

    #[googletest::test]
    fn test_default_dependency_failure() {
        let config_toml = r#"
            [[tests]]
            name = "A"
            command = ["echo", "A"]
            depends_on = ["B"]

            [[tests]]
            name = "B"
            command = ["echo", "B"]
            run_by_default = false
        "#;
        let config: Config = toml::from_str(config_toml).unwrap();

        // Case 1: No filters. A depends on B. B is excluded. Should fail.
        let res = ParsedConfig::new(config.clone(), "/fake", Vec::<&str>::new(), Vec::<&str>::new());
        assert_that!(res, err(anything())); // Should be NoSuchChild error context

        // Case 2: Explicitly include A. B is excluded. Should fail.
        let res = ParsedConfig::new(config.clone(), "/fake", Vec::<&str>::new(), vec!["A"]);
        assert_that!(res, err(anything()));

         // Case 3: Explicitly include A and B. Should pass.
        let res = ParsedConfig::new(config, "/fake", Vec::<&str>::new(), vec!["A", "B"]);
        assert_that!(res, ok(anything()));
    }
}
