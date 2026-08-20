# Contributing

PRs are welcome. This document exists so that a rejection is never a surprise.

Security issues go to [`SECURITY.md`](SECURITY.md), not the public tracker.

## Before a large change, open an issue

Small fixes — a bug, a test, a doc correction — just send the PR. For anything that
changes an interface, adds a dependency, or touches the two invariants below, open
an issue first. A design disagreement is much cheaper to resolve in an issue than
in a finished branch, and the alternative is a well-written PR closed for reasons
that had nothing to do with its quality.

## The two invariants

These are the reason the project exists, and a PR that relaxes either will be
rejected even if it is otherwise good:

1. **The admin plane's boundary is the credential, not reachability.** A session
   shell can reach the listener over loopback. Nothing may treat "inside the
   container" as authorization, and no in-container admin socket may be added.
2. **Teardown is best-effort, and says so.** Tier 1 is the only kill domain. If you
   strengthen teardown, do not also change the surfaced language to imply a
   guarantee that the implementation does not provide.

If you think an invariant is wrong, argue that in an issue. That is a legitimate
position; changing it silently in a diff is not.

## What a good PR looks like

- **One concern.** Two unrelated fixes are two PRs.
- **A test that fails before your change.** For a bug fix this is the main evidence
  the bug existed. `runtime/tests/` has end-to-end examples that drive the real
  listener, which is usually the right level for anything touching auth or sessions.
- **Existing tests still pass**, including the adversary tests. If one of those now
  fails, do not adjust the assertion to match your change — that assertion *is* the
  security claim. Explain why the claim should change instead.
- **No new dependency without a reason in the PR body.** Dependencies are supply
  chain, and this crate stays MIT-compatible: no copyleft.
- **Comments explain why, not what.** The code says what.

## Running the tests

The runtime links against musl for a static binary, so it is not built on a
laptop:

```bash
cd runtime
cargo test                      # unit + integration
cargo clippy -- -D warnings     # CI enforces this
cargo fmt --check               # and this
```

CI runs the same three on every PR, plus a matrix image build over every agent
variant. A PR does not push images — the build runs to prove the base image and
the musl link step still work.

If a test fails on `ubuntu-24.04` but passes on `macos-14`, suspect a real
platform difference before suspecting flakiness. The kill domain has a
Linux-only pidfd path that the other target cannot exercise at all, and the one
test previously written off as timing-sensitive turned out to be signalling
unrelated host processes by pid on Linux. Say which target failed in the PR.

## Commits and PRs

- Conventional-commit prefixes (`feat:`, `fix:`, `test:`, `docs:`, `chore:`).
- Write the body for someone reading it in a year with no memory of the
  conversation: what was wrong, why this fixes it, what you verified.
- Rebase on `main` rather than merging it in.
- Say what you actually ran. "Should work" is not verification, and a PR that says
  "not compiled, string change only" is more useful than one that implies testing
  that did not happen.

## Licence

Contributions are MIT, matching [`LICENSE`](LICENSE). By opening a PR you agree
your contribution is licensed that way. There is no CLA.

Note that MIT covers this repository, not the contents of the published images —
see [`NOTICE`](NOTICE).
