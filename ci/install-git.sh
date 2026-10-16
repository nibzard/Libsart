#!/bin/sh
# Builds one pinned upstream Git for the nightly support matrix (SPEC 18.3,
# task T052).
#
# The nightly tier runs the deterministic gates against more than the
# runner's system Git: the oldest release the product claims (the floor) and
# the newest upstream release. Both come from the official kernel.org
# tarball, are verified against ci/git.sha256, and are built into a private
# prefix, so the runner's own Git stays untouched:
#
#   ci/install-git.sh 2.45.0
#   PATH="$PWD/ci/artifacts/git/2.45.0/bin:$PATH" ci/run-tier.sh tests nightly
#
# The build omits network and optional integrations (NO_CURL, NO_OPENSSL,
# NO_TCLTK, NO_GETTEXT, NO_PERL): the product uses local plumbing only, and
# the omission removes the OpenSSL and curl development dependencies. A
# build still needs a C compiler, make, and the zlib headers
# (apt-get install -y zlib1g-dev; the headers ship with Xcode Command Line
# Tools on macOS).
#
# GIT_BUILD_ROOT (default ci/artifacts/git under the repository) receives
# <version>/bin/git. The script prints the PATH entry to prepend.

set -eu

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
version=${1:-}
case $version in
'' | *[!0-9.]*)
    echo "usage: ci/install-git.sh <version>   (for example: ci/install-git.sh 2.45.0)" >&2
    exit 2
    ;;
esac

file_name=git-$version.tar.gz
url=https://www.kernel.org/pub/software/scm/git/$file_name

expected=$(awk -v "name=$file_name" '$2 == name { print $1 }' "$script_dir/git.sha256")
if [ -z "$expected" ]; then
    echo "ci/git.sha256 has no checksum for $file_name; add one before pinning this version" >&2
    exit 1
fi

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

root=${GIT_BUILD_ROOT:-}
if [ -z "$root" ]; then
    root=$script_dir/artifacts/git
fi
prefix=$root/$version
marker=$prefix/.margin-installed

# A completed install succeeds at once; a partial one rebuilds from scratch.
if [ -f "$marker" ] && [ "$("$prefix/bin/git" --version 2>/dev/null || true)" = "git version $version" ]; then
    echo "git $version already built in $prefix" >&2
    echo "$prefix/bin"
    exit 0
fi
rm -rf "$prefix"
mkdir -p "$prefix"

work=$(mktemp -d "${TMPDIR:-/tmp}/margin-git.XXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM

echo "building git $version from $url" >&2
curl -fsSL --retry 3 -o "$work/$file_name" "$url"

observed=$(checksum "$work/$file_name")
if [ "$observed" != "$expected" ]; then
    echo "checksum mismatch for $file_name" >&2
    echo "expected $expected" >&2
    echo "observed $observed" >&2
    exit 1
fi

tar -xzf "$work/$file_name" -C "$work"
make_flags="NO_CURL=YesPlease NO_OPENSSL=YesPlease NO_TCLTK=YesPlease NO_GETTEXT=YesPlease NO_PERL=YesPlease prefix=$prefix"
(
    cd "$work/git-$version"
    make -j"$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 1)" $make_flags all
    make $make_flags install
) >&2

installed=$("$prefix/bin/git" --version)
if [ "$installed" != "git version $version" ]; then
    echo "the built git reports '$installed', not 'git version $version'" >&2
    exit 1
fi
touch "$marker"

echo "$prefix/bin"
