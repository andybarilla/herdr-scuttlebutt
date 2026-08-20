#!/bin/sh
# scripts/release.sh X.Y.Z — set the version in Cargo.toml, herdr-plugin.toml and
# Cargo.lock. Commit the result on a branch and merge it; tagging the merged
# commit is a separate step (pushing the tag is what fires the release).
set -eu

version=${1:-}
case "$version" in
  *.*.*) ;;
  *) echo "usage: scripts/release.sh X.Y.Z" >&2; exit 1 ;;
esac

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
root="$script_dir/.."
cd "$root"

# Anchored to ^version so herdr-plugin.toml's min_herdr_version is left alone.
sed "/^\[package\]/,/^\[/ s/^version = .*/version = \"$version\"/" Cargo.toml > Cargo.toml.tmp
mv Cargo.toml.tmp Cargo.toml
sed "s/^version = .*/version = \"$version\"/" herdr-plugin.toml > herdr-plugin.toml.tmp
mv herdr-plugin.toml.tmp herdr-plugin.toml

cargo update -p scuttlebutt
"$script_dir/check-versions.sh" >/dev/null

echo "Bumped to $version. Next: commit, open a PR, and once it is merged:"
echo "  git tag v$version <merge commit> && git push origin v$version"
