#!/bin/sh
# scripts/tag-release.sh — tag the merged version bump and push it, which is what
# builds and publishes the release. Everything it checks, the release workflow
# checks too; failing here costs a second instead of a runner.
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$script_dir/.."

fail() { echo "tag-release: $1" >&2; exit 1; }

if [ -n "$(git status --porcelain)" ]; then
  fail "working tree is dirty — the tag would not match what you have here"
fi

branch=$(git rev-parse --abbrev-ref HEAD)
[ "$branch" = main ] || fail "on $branch — release from main, after the bump PR is merged"

git fetch -q origin main
if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
  fail "main is out of sync with origin/main — pull first"
fi

version=$("$script_dir/check-versions.sh")
tag="v$version"

if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  fail "$tag already exists locally"
fi
if git ls-remote --exit-code --tags origin "$tag" >/dev/null 2>&1; then
  fail "$tag already exists on origin"
fi

git tag "$tag"
git push origin "$tag"
echo "Pushed $tag — the release workflow is building it."
