#!/bin/sh
# scripts/release.sh X.Y.Z — branch, set the version in Cargo.toml,
# herdr-plugin.toml and Cargo.lock, and commit. Open a PR with that commit; once
# it is merged, scripts/tag-release.sh publishes the release from main.
set -eu

version=${1:-}
case "$version" in
  *.*.*) ;;
  *) echo "usage: scripts/release.sh X.Y.Z" >&2; exit 1 ;;
esac

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$script_dir/.."

if [ -n "$(git status --porcelain)" ]; then
  echo "release: working tree is dirty — commit or stash first" >&2
  exit 1
fi

git checkout -q -b "release-$version"

# Anchored to ^version so herdr-plugin.toml's min_herdr_version is left alone.
sed "/^\[package\]/,/^\[/ s/^version = .*/version = \"$version\"/" Cargo.toml > Cargo.toml.tmp
mv Cargo.toml.tmp Cargo.toml
sed "s/^version = .*/version = \"$version\"/" herdr-plugin.toml > herdr-plugin.toml.tmp
mv herdr-plugin.toml.tmp herdr-plugin.toml
sed "s|^herdr plugin install andybarilla/herdr-scuttlebutt --ref v.*|herdr plugin install andybarilla/herdr-scuttlebutt --ref v$version|" README.md > README.md.tmp
mv README.md.tmp README.md

cargo update -p scuttlebutt
"$script_dir/check-versions.sh" >/dev/null
git commit -qam "chore: release $version"

echo "Committed the $version bump on release-$version."
echo "Open a PR; once it is merged, run scripts/tag-release.sh from an up-to-date main."
