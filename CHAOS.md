WELCOME TO THE LIMMAT WIKI GUESTS ARE ADVISED TO TAKE THEIR ADHD AND/OR ANXIETY
MEDICATION PRIOR TO ENTRY PLEASE DO NOT FEED THE BULLET POINTS PLEASE AVOID
EYE CONTACT WITH THE UNPUNCTUATED ALLCAPS INTRODUCTION

## Bugs (high to low priority):

 - `--http-sockaddr=localhost:8080` still gives you a hostname-based URL.
 - No integration tests for `test` subcommand.
 - No integration tests for the UI.
 - No tests for checking config cache...
 - No tests for actual contents of config cache. (E.g: Nothing to catch bug
   where we deleted stdouts and stderrs).
 - Has like a billion dependencies, they can't all be necessary.
 - Unimportant bug: some tests get run twice by `cargo test`, because of
   `test_log`/`test_case` interaction.

## Needed features (high to low priority):

Stuff that would let me clean up examples and the README:

 - Probably want a (default?) option to merge stderr and stdout.
   I started implementing this as a `get` command but there's a few things to
   think through carefully.
    - What should the CLI be? I think `get` was a bit weird. Usecases are:
      - Testing Limmat
      - Grabbing cached artifacts.
      - Checking the result of tests in scripts
      The artifacts one is the most "real" I think. But, we don't even support
      saving artifacts yet. Next question is - I think this command should fail
      when the relevant test failed. Should it just repeat the exit code of the
      test? I think so,  but arguably it's kinda confusing because you can't
      tell if that's the exit code produced by limmat or by the test.
 - Support configuring a shell, with the default based on the user's
   system-level configuration (`getent`).
 - Need a way to garbage-collect old database entries.
 - Probably need to have the system handle cleaning the worktree for you. If
   your build system etc can't be trusted to avoid polluting the workspace/being
   resilient against a polluted workspace, you'll wanna put `git clean -fdx` in
   your test script. However, once we have the `test` subcommand we'll also be
   running in the "main" worktree where the user probably doesn't wanna do that.
   So we probably need a higher-level notion of "cleaning the worktree" that's
   aware of this.

Stuff that seems fun and useful:

 - Provide a
   [jobserver](https://www.gnu.org/software/make/manual/html_node/Job-Slots.html).
   Issue with this will be when test commands crash and leak job slots. I think
   a reasonable workaround for that would just be to reset the slot count when
   the test manager becomes `settled` (this assumes that all test scripts can
   make progress on a single thread when the job server starves them, as is the
   case for Make, since all jobs have one implicit job slot).

Other stuff that is either less important (towards the bottom) or just seems
annoying to figure out (near the top):

 - Need a way to install it without `cargo`.
 - Need a way to view stderr from web UI.
 - Need a way to delete stored results.

   (Or do we? If we had an error reporting
   mechanism then there would be no need for this since you'd just modify the
   configuration to adopt the error reporting, and in that case the cache would
   be invalidated anyway. But, also need to consider cases where something was
   wrong in the host system)
 - Need timeouts! (There is a shutdown grace period, so we don't just leak
   resources if tests get stuck forever, they'll get cancelled when te user needs
   to run a new test. But we should also notify the user if they don't seem to
   be getting any test results.)
 - Would be nice to have a way to make the result cache invalidation logic aware
   of config (or other) files that the test command refers to.
   Perhaps one way to do this would be to have something like:

   ```toml
   [[config_repos]]
   name = "kernel"
   path = "path/to"  # Or remote URL - anything Git can clone
   rev = "master"    # Optional. Watched for updates?
   [[tests]]
   name = "uses_config"
   config_repos = ["kernel"]
   command = "something_with.sh --config=$LIMMAT_CONFIG_REPO_kernel"
   ```
   In this case, does the test get an exclusive copy of the config repo? Should
   that be configurable?
 - Maybe a "skipped" status that doesn't show up in the UI would be useful.
 - Sometimes you have a test that needs access to the worktree but not
   exclusive. In that case we could run multiple jobs in parallel in the same
   worktree.

   ACTUALLY - I bet we could do the same thing with overlayfs even if the jobs
   _did_ need exclusive access to the worktree.
 - Presumably via cgroups it's reasonably to ensure that jobs don't leak child
   processes. If you have a backdoor like the Docker daemon then that isn't
   possible but normally it should be fine I think?
 - It might also be useful to have a way to limited the resources used by the
   test jobs without having to also throttle the main limmat process.
 - Make it easier to share configs. At present the distinction between config
   file content and arg content may be a mit messy (e.g. `num_worktrees` is as
   much a property of the system running the service as the project being
   tested).
 - Maybe people who are sharing configs would want to be able to make them more
   hermetic, so it might make sense to integrate this with some sort of
   container runtime. However, probably it's also just easy to put a `podman
   run` command in your config or whatever? That way your hermetic configuration
   can also easily be reused in proper CI.
 - Support re-using existing worktrees, instead of blocking startup while they
   are created? I dunno though maybe that's pointless with the `test` comamnd.
   Starting the tool up shouldn't need to be something you do very often after
   we have that.
 - Provide option to run some tests inclusively of the base commit? Maybe
   overkill, user can just add `^`
 - Support multiple repos?
 - Respect git's color configuration.
 - Do we want a command like `limmat test` but that runs _all_ tests, and which
   interacts with the result DB? Usecases:
    - Integration testing limmat
    - Using limmat.toml as your CI config
   Are these good usecases? I think probably not so I'm hesitant to add this API
   surface.

Probably sketchy design choices:

 - Test scripts are shut down by sending `SIGTERM` to the _whole process group_.
   The rationale for this is that it's kinda annoying to write a bash script
   that actually shuts down cleanly on `SIGTERM` otherwise - if you do any
   background stuff then it will just leak.

   I think this is probably just a symptom of me not really knowing how to do
   Unix programming properly, I dunno. It probably needs to change.

My janky test command:

```
RUST_LOG_STYLE=always RUST_LOG=debug cargo watch -- bash -c "cargo test --color=always -- |& less -R -F -c"
```

## Documenting the config schema

I thought I was terribly clever because I was able to generate a JSON schema for
the configuration, I thought it would make it very easy to generate docs for
that schema.

Unfortunately, it isn't. The internet is strewn with tools that generate static
HTML from your JSON schema, all of them seem to be useless. Everything requires
you to use `npm` or `yarn` or `pip` (I know this tool currently requires you to
use `cargo`, but I don't think that's OK). Many don't work at all. Most seem to
generate stupid HTML with animations and silly nonsense like that.
[`json-schema-static-docs`](https://tomcollins.github.io/json-schema-static-docs/)
seems to generate nice HTML, but its UI seems to be "write some JavaScript"
so... fuck that.

I will not be generating nice HTML for the config schema.

## Running Github Actions

Do we need Github Actions? Ugh I guess. But now it's super annoying because you
can't run it locally. Try this:

```sh
git clone https://github.com/nektos/act.git
cd act/
make build

systemctl --user start podman.socket
export DOCKER_HOST=unix://$XDG_RUNTIME_DIR/podman/podman.sock

cd $LIMMAT_REPO
$ACT_REPO/dist/local/act
```

But... https://github.com/nektos/act/issues/107

I dunno whatever. Maybe I just have to live with the Github bullshit?

## Running tests in a container

But.. I was able to repro the GHA failures with this:

```
FROM ubuntu:latest
RUN apt update
RUN apt install -y curl build-essential git
RUN git config --global user.email "grundbert@example.com"; git config --global user.name "Grundbert Schnlörber"
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs > rustup.sh
RUN sh rustup.sh -y
ENV PATH="/root/.cargo/bin:${PATH}"
COPY . /limmat
WORKDIR /limmat
CMD script -e -c "cargo test"
```

```
podman build . -t limmat-tests && podman run limmat-tests
```

## Running tests

To run this as a low priority on Linux, try prefixing the command with `chrt -i
0` which will run it as `SCHED_IDLE`. `nice -n 19` isn't really enough because you probably have
[autogroups](https://man7.org/linux/man-pages/man7/sched.7.html) enabled. To run
at background priorities other than `SCHED_IDLE` you'll need to work around that
- the man page gives an example of running `echo 10 > /proc/self/autogroup` to
set `nice 10` for the current shell.

To run the tests very hard, follow
[this](https://askubuntu.com/questions/162229/how-do-i-increase-the-open-files-limit-for-a-non-root-user)
then run `ulimit -Sn 524288` to embiggen the file descriptor limit. Now you can
try using `cargo-stress`.

## Releases

In theory, releases are set up with
[release-plz](https://release-plz.ieni.dev/docs). My understanding of this
system is honestly pretty shaky, but I was interested to try it out. IIUC the
necessary tokens are stored as GitHub secrets and it should just automatically:

- Send a PR to do the in-repo changes that are needed for a release
- Keep the PR up to date as `master` develops
- After you merge the PR, automatically push the release to crates.io.

For this to work you are supposed to write ["conventional commit
messages"](https://www.conventionalcommits.org/en/v1.0.0/#summary). Let's see.

It seems like if you want some manual control over this you can just edit
Cargo.* and CHANGELOG.md manually (or using `release-plz update`/`release-plz
set-version`) and push that, as an alternative to the release PR. Then the GH
Action will just push to crates.io.

I looked into packaging this for Debian but realised it's a whole big deal. So I
guess we could have .deb files hostedon GitHub, seems fine, although .deb
doesn't do very much when the tool is just a single binary. It looks like the
best way to build `.deb` files by far is
[cargo-deb](https://crates.io/crates/cargo-deb). But it seems like setting
this up to run automatically in GHA is pretty annoying. I found
[this](https://github.com/marketplace/actions/rust-cargo-deb-package-build-amd64-ubuntu)
action which apparently runs cargo-deb and
[this](https://github.com/marketplace/actions/github-action-publish-binaries)
which apparently publishes build artifacts to GH releases, but it's not clear to
me how you're supposed to plumb these things together. I'm not too sure if I
care either. I used to manually build them and upload them to the Releases page
in the Github UI. I can no longer be bothered to do that. Here's how I used to:

- `cargo build -r                        # Upload this as limmat-linux-amd64`
- `cargo install cargo-deb && cargo deb  # This will already have the correct filename`