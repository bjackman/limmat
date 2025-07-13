# Limmat: Local Immediate Automated Testing

Limmat watches a Git branch for changes, and runs tests on every commit, in
parallel. It's a bit like having good CI, but it's all local so you don't need
to figure out infrastructure, and you get feedback faster.

It gives you a live web (and terminal) UI to show you which commits are passing
or failing each test:

![screenshot of UI](docs/assets/screenshot.png)

Clicking on the test results ([including in the
terminal](https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda))
will take you to the logs.

## Installation

> [!NOTE]
> Limmat works on Linux on x86. It _probably_ works on other architectures too.
> It's also been reported to work on MacOS.

### From crates.io

[Install
Cargo](https://doc.rust-lang.org/cargo/getting-started/installation.html) then:

```sh
cargo install limmat
```

### Via Nix

[Install Nix](https://nixos.org/download/) then use the default output of the flake defined in this
repository, for example:

```sh
nix run github:bjackman/limmat -- --help
```

### From GitHub Releases

There are pre-built Linux x86 binaries in the [latest GitHub
Release](https://github.com/bjackman/limmat/releases/latest).

If you prefer to have the tool tracked by your package manager, you can download
a `.deb` from there and install it with `dpkg -i $pkg.deb`. Or you can just
download the raw binary, it has no dependencies.

## Usage

Write a config file (details [below](#configuration)) in `limmat.toml` or `.limmat.toml`, and
if the branch you're working on is based on `origin/master`, run this from the root
of your repository:

```sh
limmat watch origin/master
```

Limmat will start testing every commit in the range `origin/master..HEAD`.
Meanwhile, it watches your repository for commits being added to or removed from
that range and spawns new tests or cancels them as needed to get you your
feedback as soon as possible.

By default tests are run in separate [Git worktrees](https://git-scm.com/docs/git-worktree).

If you don't want to store the config in the repo, put it elsewhere and point to
it with `--config`. Alternatively you can run Limmat from a different directory
and point to the repository with `--repo`.

> [!TIP]
> By default, Limmat creates worktrees in your system's default temp directory.
> If this is on `tmpfs` and your project is large, you might see "No space left
> on device" errors. You can avoid this by setting `--worktree-dir` to point to
> a disk-backed filesystem, for example `--worktree-dir /var/tmp/limmat`.

## Configuration

> [!TIP]
> For Linux kernel development, I have published an example config
> [here](https://github.com/bjackman/limmat-kernel) that you can start from.

Configuration is in [TOML](https://toml.io/en/). Let's start with an example,
here's how you might configure a Rust project (it's a reduced version of [this
repository's own config](limmat.toml)):

```toml
# Check that the formatting is correct
[[tests]]
name = "fmt"
command = "cargo fmt --check"

# Check that the tests pass
[[tests]]
name = "test"
command = "cargo test"
```

If the command is a string, it's executed via the shell. If you want direct
control of the command then pass it as a list:

```toml
[[tests]]
name = "test"
command = ["cargo", "test"]
```

### Writing the test command

The test command's job is to produce a zero (success) or nonzero (failure) status
code. By default, it's run from the root directory of a copy of the repository,
with the commit to be tested already checked out.

If your test command is nontrivial, test it with `limmat test
$test_name`. This runs it immediately in the main worktree and print its output
directly to your terminal.

> [!WARNING]
> Limmat doesn't clean the source tree for you, it just does `git checkout`. If
> your test command can't be trusted to work in a dirty worktree (for example,
> if you have janky Makefiles) you might want it to start with something like
> `git clean -fdx`. **But**, watch out, because when you run that via `limmat test`,
> it will wipe out any untracked files from your main worktree.

If your test command doesn't actually need to access the codebase, for example
if it only cares about the commit message, you can set `needs_worktree = false`.
In that case it will run in your main worktree, and the commit it needs to test
will be passed in the [environment](#job-environment) as `$LIMMAT_COMMIT`.

> [!NOTE]
> Tests configured with `command` are currently hard-coded to use Bash as the
> shell. There's no good reason for this it's just a silly limitation of the
> implementation.

When the test is no longer needed (usually because the commit is no longer in
the range being watched), the test command's process group will receive
`SIGTERM`. It should try to shut down promptly so that the worktree can be
reused for another test. If it doesn't shut down after a timeout then it will
receive `SIGKILL` instead. You can configure the timeout by setting
`shutdown_grace_period_s` in seconds (default 60).

### Caching

Results are stored in a database, and by default Limmat won't run a test again
if there's a result in the database for that commit.

You can disable that behaviour for a test by setting `cache = "no_caching"`;
then when Limmat restarts it will re-run all instances of that test.

Alternatively, you can crank the caching _up_ by setting `cache = "by_tree"`.
That means Limmat won't re-run tests unless the actual repository contents
change - for example changes to the commit message won't invalidate cache
results.

If the test is terminated by a signal, it isn't considered to have produced a
result: instead of "success" or "failure" it's an "error". Errors aren't cached.

You can also trigger un-cached "error" results yourself, by setting
`error_exit_codes` and then returning one of those codes from your command. For
example:

```toml
[[tests]]
name = "container_build"
error_exit_codes = [123]
command = """
# Don't pollute the database we if we run on a machine without Podman installed.
which -s podman || exit 123
podman build .
"""
```


The configuration for each test and its dependencies are hashed, and if this
hash changes then the database entry is invalidated.

> [!WARNING]
> If your test script uses config files that aren't checked into your repository,
> Limmat doesn't know about that and can't hash those files. It's up to you
> to determine if your scripts are "hermetic" - if they aren't you probably just want 
> to set `cache = "no_caching"`.

### Resources

If you're still reading, you probably have a lot of tests to run, otherwise you
wouldn't find Limmat useful. So the system needs a way to throttle the
parallelism to avoid gobbling resources. The most obvious source of throttling is
the worktrees. If your tests need one - i.e. if you haven't set `needs_worktree =
false` - then those tests can only be parallelised up to the `num_worktrees`
value set in your config (default: 8). But there's also more flexible throttling
available.

To use this, define `resources` globally (separately from `tests`) in your
config file, for example:

```toml
[[resources]]
name = "pokemon"
tokens = ["moltres", "articuno", "zapdos"]
```

Now a test can refer to this resource, and it won't be run until Limmat can
allocate a Pokemon for it:

```toml
[[tests]]
name = "test_with_pokemon"
resources = ["pokemon"]
command = "./test_with_pokemon.sh --pokemon=$LIMMAT_RESOURCE_pokemon"
```

As you can see, resource values are passed in the
[environment](#job-environment) to the test command.

Resources don't need to have values, they can also just be anonymous tokens for
limiting parallelism:

```toml
resources = [
    "singular_resource", # If you don't specify a count, it defaults to 1.
    { "name" = "threeple_resource", count = 3 },
]
```

### Test dependencies

Tests can depend on other tests, in which case Limmat won't run them until the
dependency tests have succeeded for the corresponding commit:

```toml
[[tests]]
name = "build-prod"
command = "make"

[[tests]]
name = "run-tests"
# No point in trying to run the tests if the prod binary doesn't compile
depends_on = ["build-prod"]
command = "run_tests.sh"
```

### Artifacts

Tests can produce output files, called _artifacts_, and other tests can access
the artifacts of tests that they depend on. Your test command is passed a
directory in `$LIMMAT_ARTIFACTS` where it can drop artifact files. Tests with
dependencies are passed the artifact directories of the dependency jobs in
`$LIMMAT_ARTIFACTS_<test name>`.

Artifacts are stored in the result database, they can be found via the `limmat
artifacts <test> <revision>` command, which prints the directory that would have
been passed in `$LIMMAT_ARTIFACTS` when the given test was run for the given
revision.

Example use-cases for this:

1. To store extra info from test runs, for example traces or debug data:

   ```toml
   [[tests]]
   name = "strace_example"
   # strace logs can be handy for debugging odd test failures, but too verbose
   # to write to stdout, so drop one into the artifacts output.
   command = "strace -f -o $LIMMAT_ARTIFACTS/strace.log ./run_tests.sh"
   ```

2. To separate your "build" and "test" jobs. For example, if your test job
   requires a resource, you might want a separate build job to build the code
   without holding onto that resource, to increase your overall test bandwidth.


   ```toml
   [[resources]]
   name = "hw_device"
   tokens = ["device1.board-farm.example.com", "device2.board-farm.example.com"]

   # Build in a separate job, which doesn't require access to a test device. Put
   # compilation output in the artifacts directory.
   [[tests]]
   name = "build"
   command = "make -j64 O=$LIMMAT_ARTIFACTS"
   
   # Run the tests on a test device, using the output of the "build" job.
   [[tests]]
   name = "test_on_hw"
   resources = ["hw_device"]
   depends_on = ["build"]
   command = """
   ./run_tests.sh  --target-host=$LIMMAT_RESOURCES_hw_device \
       --build-dir=$LIMMAT_ARTIFACTS_build
   """
   ```

3. To use Limmat's result database as a "cache". For example, when running a Git
   bisection, instead of re-building your code at every revision, if you have a 
   "build" Limmat job, grab the pre-built binary from `$(limmat artifacts build
   HEAD)` at each bisection step.
   
> [!WARNING]
> Limmat doesn't yet have logic to prune the result database, if you drop very large
> files into `$LIMMAT_ARTIFACTS` you can fill up your disk quite quickly.

### Reference

#### Config file

The JSON Schema is [available in the
repo](https://github.com/bjackman/limmat/blob/master/limmat.schema.json). (The
configuration is is TOML, but TOML and JSON are equivalent for our purposes
here. Limmat might accept JSON directly in a later version, and maybe other
formats like YAML). There are online viewers for reading JSON Schemata more
easily, try [viewing it in Atlassian's tool
here](https://json-schema.app/view/%23?url=https%3A%2F%2Fraw.githubusercontent.com%2Fbjackman%2Flimmat%2Frefs%2Fheads%2Fmaster%2Flimmat.schema.json).
[Contributions are
welcome](https://github.com/bjackman/limmat/commit/3181929c0f9031dbe9b13ad07a52b66f2f3439a4)
for static HTML documentation.

#### Job environment

These environment variables are passed to your job.

| Name                                  | Value                                                                                     |
| ------------------------------------- | ----------------------------------------------------------------------------------------- |
| `LIMMAT_ORIGIN`                       | Path of the main repository worktree (i.e. `--repo`).                                     |
| `LIMMAT_COMMIT`                       | Hash of the commit to be tested.                                                          |
| `LIMMAT_CONFIG`                       | Path of the config file.                                                          |
| `LIMMAT_RESOURCE_<resource_name>_<n>` | Values for [resources](#resources) used by the test.                                      |
| `LIMMAT_RESOURCE_<resource_name>`     | If the test only uses one of a resource, shorthand for `LIMMAT_RESOURCE_<resource_name>_0` |
| `LIMMAT_ARTIFACTS_<job_name>`         | If the test depends on `job_name`, this directory contains that job's [artifacts](#artifacts). |

### Advanced example

Here's a fictionalised example showing all the features in use at once, based on
the configuration I use for my Linux kernel development at Google.

```toml
# Don't DoS the remote build service
[[resources]]
name = "build_service"
count = 8

# Physical hosts to run tests on, with two different CPU microarchitectures.
[[resources]]
name = "milan_host"
tokens = ["milan-a8", "milan-x3"]
[[resources]]
name = "skylake_host"
tokens = ["skylake-a8", "skylake-x3"]

# Build a kernel package using the remote build service.
[[tests]]
name = "build_remote"
resources = ["build_service"]
command = "build_remote_kernel.sh --commit=$LIMMAT_COMMIT"
# build_remote_kernel.sh doesn't actually need to access the code locally, it
# just pushes the commit to a Git remote.
requires_worktree = false

# Let me know when I break the build for Arm.
[[tests]]
name = "build_remote"
resources = ["build_service"]
command = "build_remote_kernel.sh --commit=$LIMMAT_COMMIT --arch=arm64"
requires_worktree = false


# Also check that the build works with the normal kernel build system.
[[tests]]
name = "kbuild"
command = """
set -e

# The kernel's Makefiles are normally pretty good, but just in case...
git clean -fdx

make -j defconfig
make -j16 vmlinux O=$LIMMAT_ARTIFACTS
"""

# Check the locally-built kernel boots in a QEMU VM.
[[tests]]
name = "boot_qemu"
depends_on = ["kbuild"]
# check_qemu_boot.sh is an external tool (not checked into my kernel repo)
# So we don't need a worktree to run it. That means we don't need a worktree
# at all for this job, so we can free one up to run another test job.
requires_worktree = false
command = "check_qemu_boot.sh --build-dir=$LIMMAT_ARTIFACTS_kbuild"

# Check the kernel boots on an AMD CPU.
[[tests]]
name = "boot_milan"
resources = ["milan_host"]
command = "boot_remote_kernel.sh --commit=$LIMMAT_COMMIT --host=$LIMMAT_RESOURCE_milan_host"
# boot_remote_kernel.sh will just use the kernel built be build_remote_kernel.sh
# so it doesn't need to access anything locally.
requires_worktree = false
# But we must only run this test once that remote build is finished.
depends_on = ["build_remote"]

# Same as above, but on an Intel CPU.  This simplified example isn't bad, but
# this can get pretty verbose in more complex cases.  Perhaps a later version
# will optionally support a more flexible config language like Cue, to allow
# DRY configuration.
[[tests]]
name = "boot_skylake"
resources = ["skylake_host"]
command = "boot_remote_kernel.sh --commit=$LIMMAT_COMMIT --host=$LIMMAT_RESOURCE_skylake_host"
requires_worktree = false
depends_on = ["build_remote"]

# Run KUnit tests
[[tests]]
name = "kunit"
command = "./tools/testing/kunit/kunit.py run --kunitconfig=lib/kunit/.kunitconfig"
```
