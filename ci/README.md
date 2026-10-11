# CI tier support

This directory holds the pieces a verification tier needs beyond `cargo
test` itself. The tmux pin is the first one (task T066); the tier jobs of
SPEC 18.3 are wired by their own task (T052).

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
