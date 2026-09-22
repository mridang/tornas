#!/bin/sh
# Set the release version for the whole workspace. Run by semantic-release's
# prepare step (via @semantic-release/exec) before the image and packages are
# built, so `tornas --version`, the .deb and Cargo.lock all carry it.
#   scripts/set-version.sh 1.2.3
set -eu
v=${1:?usage: set-version.sh VERSION}
case "$v" in *[!0-9A-Za-z.+-]*) echo "invalid version: $v" >&2; exit 2;; esac
cd "$(dirname "$0")/.."
# Only the version key inside [workspace.package]; every crate inherits it.
perl -0pi -e 's/(\[workspace\.package\][^\[]*?\nversion\s*=\s*)"[^"]*"/${1}"'"$v"'"/' Cargo.toml
grep -Eq "^version = \"$v\"$" Cargo.toml || { echo "failed to set version in Cargo.toml" >&2; exit 1; }
# Refresh only the workspace's own entries in the lock file; the release build uses --locked.
cargo update --workspace --quiet
grep -A1 '^name = "tornas"$' Cargo.lock | grep -q "version = \"$v\"" || { echo "Cargo.lock not updated" >&2; exit 1; }
echo "version set to $v"
