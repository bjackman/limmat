# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Limmat is a Rust-based tool for local automated testing of Git branches. It watches Git repositories for changes and runs tests on every commit in parallel, providing a live web and terminal UI for test results.

## Coding style

Don't add any unused code.

Add comments for code whose intention might not be obvious or where there are
complex sections that benefit from a summary. Public APIs of nontrivial logic
should have its behaviour fully documented. Otherr than this, prefer to avoid
comments. Absolutely never add comments that just re-state the code in English,
assume that the reader knows Rust.

## Key Commands

Run everything via `nix develop`, e.g. `nix develop -c cargo build`.

### Development
- `cargo build` - Build the project
- `cargo check` - Quick compile check without code generation
- `cargo test` - Run all tests
- `cargo test -- --nocapture` - Run tests with output
- `cargo clippy` - Run linter
- `cargo clippy --all-targets -- -D warnings` - Run clippy with warnings as errors
- `cargo fmt` - Format code
- `cargo fmt --check` - Check formatting without modifying files

### Testing Single Features
- `cargo test <test_name>` - Run specific test by name
- `cargo test --test integration_test` - Run integration tests

### Running Limmat
- `cargo run -- --help` - Show help
- `cargo run -- watch origin/master` - Basic usage to watch commits
- `cargo run -- test <test_name>` - Run specific test immediately

## Architecture

### Core Modules
- `main.rs` - CLI interface and application entry point
- `config.rs` - Configuration parsing and validation (TOML-based)
- `git.rs` - Git operations, worktree management
- `test.rs` - Test execution and management
- `database.rs` - Result storage and caching
- `dag.rs` - Dependency graph management for tests
- `resource.rs` - Resource allocation and throttling
- `process.rs` - Process execution and lifecycle management
- `ui.rs` - Terminal UI components
- `http.rs` - Web UI server

### Key Concepts
- **Worktrees**: Tests run in separate Git worktrees for isolation
- **Resources**: Named tokens for controlling test parallelism (configured in `limmat.toml`)
- **Test Dependencies**: Tests can depend on other tests completing first
- **Artifacts**: Tests can produce output files accessible to dependent tests
- **Caching**: Results cached by commit hash or tree hash to avoid re-running

### Configuration
- Main config: `limmat.toml` or `.limmat.toml`
- Schema available in `limmat.schema.json`
- Tests defined in `[[tests]]` blocks with commands, dependencies, resources
- Self-testing config in `limmat.toml` shows practical examples

### Dependencies
- Built on Tokio async runtime
- Uses `clap` for CLI parsing
- `notify` for filesystem watching
- `axum` for web UI
- `serde`/`toml` for configuration
- `nix` crate for Unix process management

## Common Development Patterns

### Adding New CLI Commands
Commands are defined using `clap` derive macros in `main.rs`. Follow the existing `Subcommand` enum pattern.

### Test Execution Flow
1. Config parsing (`config.rs`)
2. Git range analysis (`git.rs`) 
3. Test dependency resolution (`dag.rs`)
4. Resource allocation (`resource.rs`)
5. Process spawning (`process.rs`)
6. Result storage (`database.rs`)

### Error Handling
Uses `anyhow` for error handling throughout. Context should be added at appropriate levels using `.context()`.