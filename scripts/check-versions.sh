#!/bin/sh
# scripts/check-versions.sh [vX.Y.Z] — the version lives in Cargo.toml (the source
# of truth), herdr-plugin.toml, Cargo.lock and the release tag; they must agree.
# Prints the version. Pass a tag name to check that too.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

cargo_version=$(sed -n '/^\[package\]/,/^\[/{s/^version = "\(.*\)"/\1/p;}' "$root/Cargo.toml" | head -n 1)
plugin_version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/herdr-plugin.toml" | head -n 1)
lock_version=$(awk '/^name = "scuttlebutt"$/ {getline; gsub(/^version = "|"$/, ""); print; exit}' "$root/Cargo.lock")

fail() { echo "check-versions: $1" >&2; exit 1; }

[ -n "$cargo_version" ] || fail "no [package] version in Cargo.toml"
[ "$plugin_version" = "$cargo_version" ] || fail "herdr-plugin.toml $plugin_version != Cargo.toml $cargo_version"
[ "$lock_version" = "$cargo_version" ] || fail "Cargo.lock $lock_version != Cargo.toml $cargo_version — run scripts/release.sh $cargo_version"

if [ $# -gt 0 ] && [ "${1#v}" != "$cargo_version" ]; then
  fail "tag $1 != version $cargo_version"
fi

echo "$cargo_version"
