Needs Rust >= 1.80.

To run this as a low priority on Linux, try prefixing the command with `chrt -i
0` which will run it as `SCHED_IDLE`. `nice -n 19` isn't really enough because
you probably have
[autogroups](https://man7.org/linux/man-pages/man7/sched.7.html) enabled. To run
at background priorities other than `SCHED_IDLE` you'll need to work around that
- the man page gives an example of running `echo 10 > /proc/self/autogroup` to
set `nice 10` for the current shell.

Bugs (high to low priority):

 - `Manager::job_cts` is indexed only by commit hash, but not by test name. This
   means only one job for each commit actually gets cancelled, and if any test
   is running for a given commit then other tests don't get started. This might
   actually explain the "gummed up" issue I saw below, since it would lead to
   lots of jobs running that are invisible in the status tracker, and possibly
   missed opportunities to start new jobs.
 - Cancelled jobs still get cached if they shut down via `exit` instead of being
   directly terminated by `SIGINT`. Need to actuall check cancellation status before writing to DB.
 - Linked output data gets interpreted as HTML instead of plaintext.
 - Sometimes the system gets gummed up, I'm not sure if this is just a
   status reporting issue or if the system stops making progress at at all.
   Probably should fix all the simpler bugs first then look into this some more.
   I don't see this when running against this repo, only when running on my big fat kernel tree.
 - Shutdown still does not happen cleanly on my kernel repo. At least one reason
   for this seems to be that child processes inherit the SIGINT. Another is that
   Ctrl-C just doesn't always kill the service.
 - Status output doesn't seem to get updated when tested range shrinks?
 - No tests for checking config cache...
 - No tests for actual contents of config cache. (E.g: Nothing to catch bug
   where we deleted stdouts and stderrs).
 - Unimportant bug: some tests get run twice by `cargo test`, because of
   `test_log`/`test_case` interaction.

Needed features (high to low priority):

 - Support re-using worktrees.
 - Support running tests that don't need worktrees.
 - Store output and artifacts. WIP but:
   - Provide a way to limit the size of the result cache.
   - Location of this should be configurable.
   - Probably need to split it up by tested repo.
   - Need to present it to the user in some convenient way
 - Provide a way to quickly check that tests in your configuration actually work.
 - Support other resources than worktrees and "tokens". Could e.g. be used for
   dev servers.
 - Make output results easier to reach.
 - Support saving artifacts so the user can reuse or analyze them later.
 - Fix output format, probably have to implement a pager in `ratatui`.
 - Support bailing out more quickly if the worktree teardown is too slow.
 - Support configuring a shell, with the default based on the user's
   system-level configuration (`getent`).
 - Provide a
   [jobserver](https://www.gnu.org/software/make/manual/html_node/Job-Slots.html).
   Issue with this will be when test commands crash and leak job slots. I think
   a reasonable workaround for that would just be to reset the slot count when
   the test manager becomes `settled` (this assumes that all test scripts can
   make progress on a single thread when the job server starves them, as is the
   case for Make, since all jobs have one implicit job slot).
 - Document config format.
 - Make it easier to share configs. At present the distinction between config
   file content and arg content may be a mit messy (e.g. `num_worktrees` is as
   much a property of the system running the service as the project being
   tested).
 - Support multiple repos?
 - Respect git's color configuration.
 - (Nice to have: avoid creating worktrees if they aren't actually to be used).
 - (Nice to have: let jobs that don't need worktrees start before worktrees are ready).

My janky test command:

```
RUST_LOG_STYLE=always RUST_LOG=debug cargo watch -- bash -c "cargo test --color=always -- |& less -R -F -c"
```