# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.7](https://github.com/bjackman/limmat/compare/v0.2.6...v0.2.7) - 2025-12-22

### Added

- add regex support for --skip-test and new --tests option

### Fixed

- Fix deadlock in with git COMMAND_SEM

### Other

- nix flake update
- *(deps)* bump actions/checkout from 5 to 6
- Clarify a comment
- Let Claude run cargo check

## [0.2.6](https://github.com/bjackman/limmat/compare/v0.2.5...v0.2.6) - 2025-07-13

### Added

- Add --skip-test arg

### Fixed

- Don't ignore git_binary setting

### Other

- Add Initial flake
- Add tip on setting --worktree-dir
- Fix README typo
- Add limmat-kernel link

## [0.2.5](https://github.com/bjackman/limmat/compare/v0.2.4...v0.2.5) - 2025-01-26

### Fixed

- Make shutdown a bit more responsive
- Don't send database references over the global notif channel
- Bias event-loop towards user action
- Discard revisions if more than 1024
- Fix integer underflow
- Fix terminal flickering with dumb hack
- Don't panic on dropped notifications
- Throttle the number of concurrent test jobs
- Throttle the number of git commands run in parallel

### Other

- Revert "fix: Don't hold database locks from UI code"
- Fixup failure message
- Comment on semaphore
- More verbose logging for Git output mysteries
- Fix GitHub release link
- Update stale comment
- Make Worktree::git a private method
- Turn Worktree::git into an async method
- Remove unused impl params

## [0.2.4](https://github.com/bjackman/limmat/compare/v0.2.3...v0.2.4) - 2025-01-12

### Added

- Add LIMMAT_CONFIG to job env

### Fixed

- Don't hold database locks from UI code

### Other

- Remove false fallibility
- Remove unnecessary data from OutputBuffer::render_cases
- Factor out OutputBuffer::render_case
- Support dumping child output on drop
- Shitty bullshit hacks for printf debugging in shitty tests
- cargo add --dev chrono
- Strip ansi codes when dumping stdout
- *(dev)* Document build release artifacts

## [0.2.3](https://github.com/bjackman/limmat/compare/v0.2.2...v0.2.3) - 2025-01-08

### Added

- Add error_exit_codes
- Log to file on disk
- Add --git-binary arg
- Slightly better visibility for startup/shutdown
- Return exit code 50 for nonexistent results
- Implement LIMMAT_ARTIFACTS_<dep>
- Add "artifacts" command
- Add $LIMMAT_ARTIFACTS
- Make some args global
- Implement initial database locking

### Fixed

- Fix switching to/from alternate screen
- Better message on fatal error
- Use correct test name in watch
- Use correct test name
- Try a cryptographic hash for configuration
- Retry git worktree creation
- Implement read locking too
- Implement proper database entry locking
- Recover correctly from broken database results
- Don't panic
- Note in --help that "get" is experimental

### Other

- Add anchor link
- Document LIMMAT_ARTIFACTS_x
- Words
- Add builder for tests
- Support expecting arbitrary exit codes
- Dump stderr/stdout on error too
- *(dev)* Give myself a TODO list
- *(dev)* Tick off some completed tasks
- Avoid N git commands for building UI buffer
- Return raw bytes from Worktree::log
- Add style argument to Worktree::log
- Shrink OutputBuffer::new
- Drop debug logs
- Finish documenting artifacts
- Spellcheck README
- Reapply "doc: Partially document artifacts"
- Create repos in LimmatChildBuilder::new
- Use fixed git binary in integration tests
- Revert "test: Embiggen some test timeouts"
- Rename StatusTracker->StatusViewer
- Make config hashes strings
- cargo add hex
- Embiggen some test timeouts
- Use fancy exit status to detect readiness
- Fixup awaiting readiness for clean shutdown
- Remove some unnecessary config variables
- Enable incremental mode?
- Make config an argument of builder constructor
- Make LimmatChildBuilders reusable
- Remove a debug log
- Make test_job_env multi-commit
- Smoke test for artifact env vars
- Revert "doc: Partially document artifacts"
- Partially document artifacts
- Smoke tests for dependency artifacts
- Make TestOutcome contain a DB entry
- Remove TestJobOutput trait
- Make DatabaseOutput::set_result return the created entry
- Create DatabaseOutput::ephemeral
- Make DatabaseOutput directly return Stdio
- Remove unnecessary pub
- Make DatabaseOutput::set_result consume self
- Comment on DB locking
- Revert "cleanup: Ensure no double-opened databases"
- Ensure no double-opened databases
- Integration test for test subcommand
- cargo add sha3
- Add transitive trust for cargo-vet
- Add some more cargo-vet imports
- Import google's audit and prune exceptions
- Add cargo-vet config
- Hack to make should_not_race failures easier to read
- Log config hashes
- Bring back warning about locking
- Add comment on garbage flocking
- Hacks to make race failures easier to debug
- Add a log for test status changes
- Clean up database lookup logging
- Fix clippy
- cargo fmt
- Remove unnecessary remark
- Fix bugs in dag module
- Add failing test cases for dag::tests
- Make I an associated type of trait GraphNode
- Remove warning about race conditions
- Add test for locking database entries
- More detailed errors
- Clean up TestJob notifying etc a bit
- checkpoint
- checkpoint
- Make run_inner return TestOutcome
- Make TestOutcome be a Result
- Split up TestStatus and TestOutcome
- Move output creation into TestJob::run
- *(dev)* Add warning about locking
- *(dev)* Notes on config repos
- *(dev)* Bug notes
- Fix new Clippy lints
- *(dev)* Notes on flock
- Remove timestamp argument from commit funcs
- Pull out TestJob::set_env
- Don't print noise when running 0 dep tests
- *(dev)* Notes
- Clarify `limmat test` intention
- I accidentally a word

## [0.2.2](https://github.com/bjackman/limmat/compare/v0.2.1...v0.2.2) - 2024-11-25

### Added

- Add favicon
- Add a proper title to the web UI tab
- Support finding stderr as well as stdout
- Implement 'get' command
- Add default for --config

### Fixed

- Fixup LIMMAT_RESOURCE_ env handling
- Move database checking into TestJob
- Move test result reporting into Job

### Other

- Use shared repo in another test
- Dump config & args to debug log
- Share repo between runs in should_find_output
- It works on MacOS
- *(dev)* Notes on bugs
- Use Default for default
- Add some debug logging
- More tests for 'test' command
- Remove cache_lookup helper
- Basic tests for 'test' command
- Some more logging tweaks
- Fiddle around with Job API
- Fix typos
- Allow running other subcommands with LimmatChildBuilder
- Create DatabaseEntry API
- Renames in database API
- Make Database::cached_result accept TestCase
- Stop dancing iterator quadrilles
- Break out helper to run dependency jobs
- Use async properly in integration tests
- *(dev)* Notes on features
- Fixup order of structs/impls
- I proofread them
- Notes on platforms & binaries
- *(dev)* Notes on packaging
- More installation notes
- *(dev)* Notes on release-plz experimentation

## [0.2.1](https://github.com/bjackman/local-ci/compare/v0.1.0...v0.1.1) - 2024-11-23

This is the first "proper" relase. I've added documentation and the initial featureset.