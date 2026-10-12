# CI tier support

This directory holds the pieces a verification tier needs beyond `cargo
test` itself: the tier gates of SPEC 18.3 (task T052) and the tool pins
those gates install (tmux from T066, and the Git versions of the nightly
support matrix).

The tiers themselves:

- `.github/workflows/pr-tier.yml` — the pull-request tier: lints, the
  declared Rust minimum, the deterministic test gate at 256 generated
  property cases, and the four small TLC profiles.
- `.github/workflows/nightly-tier.yml` — the nightly tier: the Git/OS
  support matrix, the 10,000-case property gate, all five TLC profiles
  plus the mutation checks, and the full-scale stress corpora.
- `ci/run-tier.sh <gate> [pr|nightly]` — every gate's commands. The
  workflows call this script; the same command runs a gate locally. Each
  run writes its log, tool versions, and timing line under
  `ci/artifacts/<gate>-<tier>/`.
- `ci/install-git.sh <version>` and `ci/git.sha256` — the pinned Git
  releases of the nightly matrix, built from the official kernel.org
  tarball into a private prefix.

The claimed Rust, Git, Linux, and macOS versions, with the evidence
behind each claim, live in
[docs/verification-tiers.md](../docs/verification-tiers.md).

## Shared build defaults

`.cargo/config.toml` turns incremental compilation off and lowers the
dev profile's debuginfo to line tables. Every gate builds cold — a
pull-request run, a nightly matrix leg, and a fresh checkout each
compile every crate once — so an incremental cache and full debuginfo
were disk and time no gate used: the one-shot
`cargo test --all-targets` target directory measured 3.4 GiB before the
defaults and 1.5 GiB after them. Backtraces keep their line numbers
(`RUST_BACKTRACE=1` is set by every gate); a session that wants full
debugging overrides the defaults per invocation:

```bash
CARGO_INCREMENTAL=1 CARGO_PROFILE_DEV_DEBUG=2 cargo test
```

## The pinned tmux

The PR and nightly images install one exact tmux so the desk composition
cases of `tests/tmux_desk.rs` always execute. Without the pin, a runner
whose image has no tmux prints the skip notice and the composition claims
of SPEC 12.6 go unexercised.

| File | Meaning |
| --- | --- |
| `tmux.version` | The exact tmux release the tiers install: 3.4 |
| `tmux.sha256` | The checksum and file name of the official tarball |
| `install-tmux.sh` | Installs the pin; succeeds at once when it is already on `PATH` |

Tier images run:

```bash
ci/install-tmux.sh
MARGIN_REQUIRE_TMUX=1 cargo test --test tmux_desk --locked
```

`MARGIN_REQUIRE_TMUX=1` turns a missing or too-old tmux into a test
failure instead of a skip, so a broken image fails loudly instead of
passing without evidence. Any value other than 0 enables the demand,
including a set-but-empty one, so a workflow template that renders an
empty string cannot silently lose its demand; a local run without the
variable keeps the courteous skip with its notice.

A build needs a C compiler, `make`, a yacc (`bison`), and the libevent
and ncurses headers (`apt-get install -y libevent-dev ncurses-dev
bison`, or `brew install libevent ncurses bison pkg-config` on macOS).
tmux's configure step asks for a yacc even though the release tarball
ships its generated parser. The script verifies the tarball checksum,
builds into `/usr/local` (or `~/.local` without write access,
overridable with `TMUX_PREFIX`), and checks the installed version.

## Why tmux 3.1 is the floor

The desk contract needs tmux 3.1 or newer, for two commands it issues:

- Direct execution: `new-session`, `new-window`, and `split-window`
  accept the command as multiple arguments after `--`, and tmux runs it
  with `execvp` and no shell, so each argument of the agent argv stays
  one argv element. The tmux manual documents this form; Margin relies
  on it and never interpolates content into a shell string.
- The side-by-side arrangement sizes the Margin pane with
  `split-window -l 55%`. Percentage sizes of `-l` were added in tmux
  3.1; tmux 3.0 rejects `-l 55%` as not a number and the desk split
  fails.

The pin is 3.4 because that is the release the composition claims were
exercised against when the desk was built. Raise it together with
`tmux.sha256` after running the desk suite against a newer release;
never lower it below 3.1.

## The pinned Git versions

The nightly matrix runs the deterministic gate against more than the
runner's system Git. `ci/install-git.sh <version>` builds one pinned
release from the official kernel.org tarball after the checksum in
`ci/git.sha256` matches, into a private prefix under
`ci/artifacts/git/<version>/` (override with `GIT_BUILD_ROOT`), and
prints the `bin` directory to put first on `PATH`. The build omits
network and optional integrations (`NO_CURL`, `NO_OPENSSL`, `NO_TCLTK`,
`NO_GETTEXT`, `NO_PERL`): the product uses local plumbing only, and a
tier image needs no OpenSSL development headers.

The pins and their roles:

| Pin | Role |
| --- | --- |
| 2.45.0 | The oldest claimed upstream Git (the floor; see `docs/verification-tiers.md` for why older releases fail) |
| 2.56.0 | The newest upstream release at the time of pinning |

Raise the floor pin only with recorded evidence: the flag probes and the
gate run against the candidate. Raise the latest pin together with
`ci/git.sha256` whenever a new upstream release should join the matrix.

