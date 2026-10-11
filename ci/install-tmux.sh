#!/bin/sh
# Installs the tmux version pinned for the CI tiers (task T066, SPEC 18.3).
#
# The PR and nightly images call this script so the desk composition cases
# of tests/tmux_desk.rs always execute against one known tmux instead of
# skipping wherever the image has none. The pin lives beside this script:
#
#   ci/tmux.version   the exact tmux release to install
#   ci/tmux.sha256    the checksum and file name of the official tarball
#
# A machine that already runs the pinned version succeeds at once. Every
# other machine builds the pinned release from the official tarball, after
# the checksum matches, into TMUX_PREFIX (default /usr/local, or ~/.local
# when /usr/local is not writable), and prints the PATH the tier needs.
#
# A build needs a C compiler, make, a yacc (bison), and the libevent and
# ncurses headers:
#   apt-get install -y libevent-dev ncurses-dev bison
#   brew install libevent ncurses bison pkg-config   (macOS)

set -eu

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
version=$(tr -d ' \t\r\n' < "$script_dir/tmux.version")
case $version in
'' | *[!0-9.]*)
    echo "ci/tmux.version does not hold a tmux release number: '$version'" >&2
    exit 1
    ;;
esac

file_name=tmux-$version.tar.gz
expected=$(awk 'NR == 1 { print $1 }' "$script_dir/tmux.sha256")
recorded_name=$(awk 'NR == 1 { print $2 }' "$script_dir/tmux.sha256")
if [ "$recorded_name" != "$file_name" ]; then
    echo "the pin files disagree: ci/tmux.sha256 names '$recorded_name' but ci/tmux.version pins $version" >&2
    exit 1
fi

# One checksum of one file, in the sha256sum output shape.
checksum() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        echo "no sha256 tool found; install sha256sum or shasum" >&2
        exit 1
    fi
}

# A machine that already runs the pinned version is done.
if command -v tmux >/dev/null 2>&1 && [ "$(tmux -V 2>/dev/null || true)" = "tmux $version" ]; then
    echo "tmux $version already on PATH"
    exit 0
fi

prefix=${TMUX_PREFIX:-}
if [ -z "$prefix" ]; then
    if [ -w /usr/local ]; then
        prefix=/usr/local
    else
        prefix=$HOME/.local
    fi
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/margin-tmux.XXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM

url=https://github.com/tmux/tmux/releases/download/$version/$file_name
echo "building tmux $version from $url into $prefix"
curl -fsSL --retry 3 -o "$work/$file_name" "$url"

observed=$(checksum "$work/$file_name")
if [ "$observed" != "$expected" ]; then
    echo "checksum mismatch for $file_name" >&2
    echo "expected $expected" >&2
    echo "observed $observed" >&2
    exit 1
fi

tar -xzf "$work/$file_name" -C "$work"
(
    cd "$work/tmux-$version"
    ./configure --prefix="$prefix"
    make -j"$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 1)"
    make install
)

installed=$("$prefix/bin/tmux" -V)
if [ "$installed" != "tmux $version" ]; then
    echo "the built tmux reports '$installed', not 'tmux $version'" >&2
    exit 1
fi

case ":$PATH:" in
*":$prefix/bin:"*) ;;
*)
    echo "note: $prefix/bin is not on PATH; put it there before running the tests" >&2
    ;;
esac
echo "installed $installed in $prefix/bin"
