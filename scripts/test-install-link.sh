#!/bin/sh
# scripts/test-install-link.sh — smoke test for the entrypoint-link behavior in
# scripts/fetch-or-build.sh. Runs the real script (copied into fixture plugin
# checkouts) against a fixture "release" served over file:// via
# SCUTTLEBUTT_BASE_URL, under a temp HOME, with a stub binary instead of a
# cargo build. Asserts on ~/.local/bin/herdr-scuttlebutt:
#   (a) fresh install creates the managed link
#   (b) reinstall from a second checkout repoints the managed link
#   (c) an unrelated regular file at that path is refused and left untouched
#   (d) an unrelated symlink at that path is refused and left untouched
#   (e) a dangling managed link (target basename `scuttlebutt`) is repointed
# Standalone: sh scripts/test-install-link.sh
set -u

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root="$script_dir/.."

for tool in curl readlink; do
  command -v "$tool" >/dev/null 2>&1 || { echo "test-install-link: $tool is required" >&2; exit 1; }
done
if command -v sha256sum >/dev/null 2>&1; then sha256=sha256sum
elif command -v shasum >/dev/null 2>&1; then sha256="shasum -a 256"
else echo "test-install-link: sha256sum or shasum is required" >&2; exit 1
fi

tmp=$(mktemp -d) || exit 1
trap 'rm -rf "$tmp"' EXIT
# Fixture checkouts are not git repos; keep the script under test's
# `git rev-parse HEAD` from wandering up into whatever worktree TMPDIR is in.
export GIT_CEILING_DIRECTORIES="$tmp"

version=$(sed -n '/^\[package\]/,/^\[/{s/^version = "\(.*\)"/\1/p;}' "$repo_root/Cargo.toml" | head -n 1)
[ -n "$version" ] || { echo "test-install-link: no [package] version in Cargo.toml" >&2; exit 1; }

# Same platform mapping as fetch-or-build.sh.
case "$(uname -s 2>/dev/null)/$(uname -m 2>/dev/null)" in
  Darwin/arm64 | Darwin/aarch64) triple="aarch64-apple-darwin" ;;
  Darwin/x86_64 | Darwin/amd64)  triple="x86_64-apple-darwin" ;;
  Linux/aarch64 | Linux/arm64)   triple="aarch64-unknown-linux-gnu" ;;
  Linux/x86_64 | Linux/amd64)    triple="x86_64-unknown-linux-gnu" ;;
  *) echo "test-install-link: unsupported platform $(uname -s)/$(uname -m)" >&2; exit 1 ;;
esac

# Fixture release server: a stub-binary tarball, a non-empty commit stamp, and
# checksums, laid out like a GitHub release and served over file://.
server="$tmp/server/v$version"
mkdir -p "$server/stage"
printf '#!/bin/sh\necho scuttlebutt stub\n' > "$server/stage/scuttlebutt"
chmod +x "$server/stage/scuttlebutt"
asset="scuttlebutt-$version-$triple.tar.gz"
tar -czf "$server/$asset" -C "$server/stage" scuttlebutt || exit 1
echo "0000000000000000000000000000000000000000" > "$server/COMMIT"
(cd "$server" && $sha256 "$asset" > SHA256SUMS) || exit 1

# A fixture plugin checkout: the real fetch-or-build.sh plus a minimal
# Cargo.toml carrying this repo's version, so the script under test resolves
# repo_root, version, and the output binary exactly as in a real install.
make_checkout() {
  mkdir -p "$1/scripts"
  cp "$repo_root/scripts/fetch-or-build.sh" "$1/scripts/fetch-or-build.sh"
  printf '[package]\nname = "scuttlebutt"\nversion = "%s"\n' "$version" > "$1/Cargo.toml"
}

home="$tmp/home"
link="$home/.local/bin/herdr-scuttlebutt"
mkdir -p "$(dirname "$link")"

run_install() { # checkout
  env HOME="$home" SCUTTLEBUTT_BASE_URL="file://$tmp/server" sh "$1/scripts/fetch-or-build.sh" >"$tmp/out.log" 2>&1
}

pass=0
fail=0
ok() { pass=$((pass + 1)); echo "ok $pass - $1"; }
not_ok() { fail=$((fail + 1)); echo "not ok - $1"; sed 's/^/    /' "$tmp/out.log"; }
expect_link() { # expected-target description
  actual=$(readlink "$link" 2>/dev/null || echo "(missing)")
  if [ -L "$link" ] && [ "$actual" = "$1" ]; then ok "$2"; else not_ok "$2 — $link -> $actual, want $1"; fi
}

c1="$tmp/checkout1"
c2="$tmp/checkout2"
make_checkout "$c1"
make_checkout "$c2"
# fetch-or-build.sh derives the binary path as $repo_root/target/release with
# repo_root = $script_dir/.. (not canonicalized), so link targets keep the
# `scripts/..` segment; assert against exactly that.
bin1="$c1/scripts/../target/release/scuttlebutt"
bin2="$c2/scripts/../target/release/scuttlebutt"

# (a) Fresh install creates the entrypoint.
if run_install "$c1"; then
  expect_link "$bin1" "fresh install creates the managed link"
  [ -x "$bin1" ] && [ "$("$bin1")" = "scuttlebutt stub" ] \
    && ok "fresh install places a runnable binary" || not_ok "fresh install places a runnable binary"
else
  not_ok "fresh install succeeded"
fi

# (b) Reinstall from a second checkout repoints the managed link.
if run_install "$c2"; then
  expect_link "$bin2" "reinstall from a second checkout repoints the managed link"
else
  not_ok "reinstall from a second checkout succeeded"
fi

# (c) An unrelated regular file is refused and left untouched.
rm -f "$link"
echo "user data" > "$link"
if run_install "$c1"; then
  not_ok "unrelated regular file is refused (install succeeded)"
else
  ok "unrelated regular file is refused (install failed)"
  if [ -f "$link" ] && [ ! -L "$link" ] && [ "$(cat "$link")" = "user data" ]; then
    ok "unrelated regular file is left untouched"
  else
    not_ok "unrelated regular file is left untouched"
  fi
  grep -qF "$link" "$tmp/out.log" \
    && ok "conflict output names the blocked path" || not_ok "conflict output names the blocked path"
fi

# (d) An unrelated symlink is refused and left untouched.
rm -f "$link"
ln -s /bin/true "$link"
if run_install "$c1"; then
  not_ok "unrelated symlink is refused (install succeeded)"
else
  ok "unrelated symlink is refused (install failed)"
  [ "$(readlink "$link")" = "/bin/true" ] \
    && ok "unrelated symlink is left untouched" || not_ok "unrelated symlink is left untouched"
  grep -qF "$link" "$tmp/out.log" \
    && ok "conflict output names the blocked path" || not_ok "conflict output names the blocked path"
fi

# (e) A dangling managed link (basename `scuttlebutt`, e.g. a deleted checkout)
# is repointed.
rm -f "$link"
ln -s "$tmp/deleted-checkout/target/release/scuttlebutt" "$link"
[ -e "$link" ] && { not_ok "fixture: dangling link fixture actually resolves"; exit 1; }
if run_install "$c1"; then
  expect_link "$bin1" "dangling managed link is repointed"
else
  not_ok "dangling managed link is repointed (install failed)"
fi

echo "1..$((pass + fail))"
[ "$fail" -eq 0 ] || { echo "test-install-link: $fail check(s) failed" >&2; exit 1; }
echo "test-install-link: all $pass checks passed"
