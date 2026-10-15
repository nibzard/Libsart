#!/usr/bin/env bash
# Runs one verification gate of the SPEC 18.3 CI tiers (task T052).
#
# Usage: ci/run-tier.sh <gate> [tier]
#
#   gate      lints | msrv | tests | formal | mutations | stress
#   tier      pr (default) | nightly
#
# The GitHub workflows under .github/workflows call this script, and the
# same command runs a gate locally. The tier argument selects the gate's
# budget where the tiers differ:
#
#   tests    pr: 256 generated property cases (SPEC 16.4) and the default
#               matrix history; nightly: 10,000 cases, 64 large cases, and
#               a 400-commit matrix history.
#   formal   pr: the four small safety, liveness, control, and negative
#               profiles; nightly: all five profiles, including the larger
#               MarginReviewNightly profile.
#
# The other gates ignore the tier.
#
# Every gate writes its log, an environment record, and a timing line under
# ${MARGIN_TIER_ARTIFACTS:-ci/artifacts}/<gate>-<tier>/ so a failed run can
# be examined (SPEC 18.4: failed seeds, Git states, traces, and raw timing
# stay retained; the property suites print the seed of every failed case,
# and each seed reproduces its case exactly). Artifacts of earlier runs are
# never deleted by this script; a new run overwrites the files it produces.
#
# Environment overrides, checked before the tier defaults:
#   MARGIN_RESTORATION_CASES, MARGIN_RESTORATION_LARGE_CASES,
#   MARGIN_MATRIX_HISTORY_COMMITS, MARGIN_BENCH_FULL_C1/C2/C3,
#   MARGIN_TIER_ARTIFACTS.

set -uo pipefail

script_dir=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
root=$(cd "$script_dir/.." && pwd)
cd "$root"

gate=${1:-}
tier=${2:-pr}
case $tier in
pr | nightly) ;;
*)
    echo "unknown tier '$tier'; expected pr or nightly" >&2
    exit 2
    ;;
esac

case $gate in
lints | msrv | tests | formal | mutations | stress | budgets) ;;
*)
    echo "usage: ci/run-tier.sh <lints|msrv|tests|formal|mutations|stress|budgets> [pr|nightly]" >&2
    exit 2
    ;;
esac

artifacts=${MARGIN_TIER_ARTIFACTS:-ci/artifacts}/$gate-$tier
mkdir -p "$artifacts"
# Subcommands may change directory before reading their arguments, so every
# gate passes absolute artifact paths from here on.
artifacts=$(CDPATH='' cd -- "$artifacts" && pwd)

# The environment record every gate keeps: the claimed tool versions of the
# machine that produced the evidence.
{
    echo "gate: $gate (tier $tier)"
    echo "started (UTC): $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    echo "git: $(git --version 2>&1) at $(command -v git || echo 'not found')"
    echo "rustc: $(rustc --version 2>&1)"
    echo "cargo: $(cargo --version 2>&1)"
    if command -v java >/dev/null 2>&1; then
        echo "java: $(java -version 2>&1 | head -1)"
    else
        echo "java: not found"
    fi
    if command -v tmux >/dev/null 2>&1; then
        echo "tmux: $(tmux -V 2>&1)"
    else
        echo "tmux: not found"
    fi
    echo "kernel: $(uname -srm)"
    if [ -r /etc/os-release ]; then
        sed -n 's/^PRETTY_NAME=/os: /p' /etc/os-release
    fi
    sw_vers -productVersion 2>/dev/null | sed 's/^/macOS: /' || true
    echo "cpus: $(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo '?')"
} >"$artifacts/environment.txt" 2>&1

started=$(date +%s)

# Runs one command with its output teed into the gate log. The exit status
# is the command's own status, not tee's.
run_logged() {
    echo "+ $*" >&2
    "$@" 2>&1 | tee -a "$artifacts/run.log"
    return "${PIPESTATUS[0]}"
}

echo "gate $gate (tier $tier); log: $artifacts/run.log" >&2
: >"$artifacts/run.log"

status=0
case $gate in
lints)
    run_logged cargo fmt --check || status=$?
    if [ "$status" -eq 0 ]; then
        run_logged cargo clippy --locked --all-targets --quiet -- -D warnings || status=$?
    fi
    ;;

msrv)
    msrv=$(sed -n 's/^rust-version[[:space:]]*=[[:space:]]*"\([^"]*\)"/\1/p' Cargo.toml | head -1)
    if [ -z "$msrv" ]; then
        echo "Cargo.toml declares no rust-version; the MSRV gate has nothing to check" >&2
        status=2
    else
        echo "declared MSRV: $msrv" | tee -a "$artifacts/run.log" >&2
        if ! rustup toolchain list 2>/dev/null | grep -q "^$msrv"; then
            echo "toolchain $msrv absent; installing" | tee -a "$artifacts/run.log" >&2
            rustup toolchain install "$msrv" --profile minimal >>"$artifacts/run.log" 2>&1 || status=$?
        fi
        if [ "$status" -eq 0 ]; then
            run_logged cargo "+$msrv" check --locked --all-targets || status=$?
        fi
    fi
    ;;

tests)
    case $tier in
    pr)
        export MARGIN_RESTORATION_CASES=${MARGIN_RESTORATION_CASES:-256}
        export MARGIN_RESTORATION_LARGE_CASES=${MARGIN_RESTORATION_LARGE_CASES:-16}
        ;;
    nightly)
        export MARGIN_RESTORATION_CASES=${MARGIN_RESTORATION_CASES:-10000}
        export MARGIN_RESTORATION_LARGE_CASES=${MARGIN_RESTORATION_LARGE_CASES:-64}
        export MARGIN_MATRIX_HISTORY_COMMITS=${MARGIN_MATRIX_HISTORY_COMMITS:-400}
        ;;
    esac
    # A tier image that lacks tmux fails the desk cases instead of skipping
    # them (see ci/README.md).
    export MARGIN_REQUIRE_TMUX=${MARGIN_REQUIRE_TMUX:-1}
    {
        echo "MARGIN_RESTORATION_CASES=$MARGIN_RESTORATION_CASES"
        echo "MARGIN_RESTORATION_LARGE_CASES=$MARGIN_RESTORATION_LARGE_CASES"
        echo "MARGIN_MATRIX_HISTORY_COMMITS=${MARGIN_MATRIX_HISTORY_COMMITS:-250}"
        echo "MARGIN_REQUIRE_TMUX=$MARGIN_REQUIRE_TMUX"
    } >>"$artifacts/environment.txt"
    export RUST_BACKTRACE=1
    run_logged cargo test --all-targets --locked --no-fail-fast || status=$?
    ;;

formal)
    if [ "$tier" = pr ]; then
        run_logged formal/run-profiles.sh "$artifacts/formal" \
            MarginReview MarginReviewLive MarginReviewCooperative \
            MarginReviewUncooperative || status=$?
    else
        run_logged formal/run-profiles.sh "$artifacts/formal" || status=$?
    fi
    ;;

mutations)
    run_logged formal/run-mutations.sh "$artifacts/mutations" || status=$?
    ;;

stress)
    export MARGIN_BENCH_FULL_C1=${MARGIN_BENCH_FULL_C1:-1}
    export MARGIN_BENCH_FULL_C2=${MARGIN_BENCH_FULL_C2:-1}
    export MARGIN_BENCH_FULL_C3=${MARGIN_BENCH_FULL_C3:-1}
    {
        echo "MARGIN_BENCH_FULL_C1=$MARGIN_BENCH_FULL_C1"
        echo "MARGIN_BENCH_FULL_C2=$MARGIN_BENCH_FULL_C2"
        echo "MARGIN_BENCH_FULL_C3=$MARGIN_BENCH_FULL_C3"
    } >>"$artifacts/environment.txt"
    export RUST_BACKTRACE=1
    run_logged cargo test --test bench_corpora --locked || status=$?
    ;;

budgets)
    # The two SPEC 17 initial rules a shared runner can enforce: the
    # stripped release executable stays within 16 MiB, and a fresh
    # `--help` process stays far inside the P-HELP budget. Shared-runner
    # timing is smoke evidence only (SPEC 17): the startup ceiling is ten
    # times the budget, so a real regression fails while runner noise
    # does not.
    run_logged cargo build --release --locked --quiet || status=$?
    if [ "$status" -eq 0 ]; then
        binary=target/release/margin
        cp "$binary" "$artifacts/margin-release-stripped" 2>>"$artifacts/run.log"
        strip "$artifacts/margin-release-stripped" 2>>"$artifacts/run.log" || true
        size_bytes=$(wc -c <"$artifacts/margin-release-stripped" | tr -d ' ')
        size_limit=$((16 * 1024 * 1024))
        p95_ms=$(python3 - "$binary" <<'PY'
import subprocess, sys, time
binary = sys.argv[1]
samples = []
for _ in range(30):
    start = time.monotonic()
    subprocess.run(
        [binary, "--help"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    samples.append((time.monotonic() - start) * 1000.0)
samples.sort()
print(f"{samples[min(len(samples) - 1, int(len(samples) * 0.95))]:.1f}")
PY
        )
        startup_limit_ms=750
        size_verdict=over
        [ "$size_bytes" -le "$size_limit" ] && size_verdict=within
        time_verdict=$(python3 -c "print('within' if $p95_ms <= $startup_limit_ms else 'over')")
        {
            printf 'metric\tvalue\tlimit\tverdict\n'
            printf 'stripped_release_bytes\t%s\t%s\t%s\n' \
                "$size_bytes" "$size_limit" "$size_verdict"
            printf 'help_p95_ms\t%s\t%s\t%s\n' \
                "$p95_ms" "$startup_limit_ms" "$time_verdict"
        } | tee "$artifacts/budgets.tsv" >&2
        [ "$size_verdict" = within ] || status=1
        [ "$time_verdict" = within ] || status=1
    fi
    ;;
esac

wall=$(( $(date +%s) - started ))
{
    printf 'gate\ttier\texit\twall_s\n'
    printf '%s\t%s\t%s\t%s\n' "$gate" "$tier" "$status" "$wall"
} >"$artifacts/summary.tsv"

echo "gate $gate (tier $tier): exit $status, ${wall}s; evidence in $artifacts" >&2
exit "$status"
