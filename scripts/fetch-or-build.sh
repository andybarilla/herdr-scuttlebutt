#!/bin/sh
# scripts/fetch-or-build.sh — herdr [[build]] step, run with cwd = plugin root.
# Downloads the prebuilt binary for this version + platform from the matching
# GitHub release and verifies its SHA-256, so installing needs no Rust toolchain.
# The release also stamps the commit it was built from, and a checkout sitting on
# any other commit gets built from source instead of handed an older binary.
# Anything that misses (unmapped platform, no release for this version, commit
# mismatch, download or checksum failure) falls back to `cargo build --release`.
set -u

repo="andybarilla/herdr-scuttlebutt"

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root="$script_dir/.."
out="$repo_root/target/release/scuttlebutt"
base_url="${SCUTTLEBUTT_BASE_URL:-https://github.com/$repo/releases/download}"

have() { command -v "$1" >/dev/null 2>&1; }

build_from_source() {
  # herdr may have been launched without ~/.cargo/bin on PATH (GUI or login-less
  # start), so pick cargo up from its env file when there is one.
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
  have cargo || {
    echo "scuttlebutt: cargo not found. Install Rust from https://rustup.rs, or re-install at a release tag (--ref v${version:-X.Y.Z}) to get a prebuilt binary." >&2
    exit 1
  }
  cd "$repo_root" || exit 1
  exec cargo build --release
}

fallback() {
  echo "scuttlebutt: $1 — building from source instead." >&2
  rm -rf "${tmpdir:-}"
  build_from_source
}

download() {
  if have curl; then curl -fsSL --retry 2 -o "$2" "$1"
  elif have wget; then wget -q -O "$2" "$1"
  else return 127
  fi
}

sha256_of() {
  if have sha256sum; then sha256sum "$1" | awk '{print $1}'
  elif have shasum; then shasum -a 256 "$1" | awk '{print $1}'
  else return 127
  fi
}

case "$(uname -s 2>/dev/null)/$(uname -m 2>/dev/null)" in
  Darwin/arm64 | Darwin/aarch64) triple="aarch64-apple-darwin" ;;
  Darwin/x86_64 | Darwin/amd64)  triple="x86_64-apple-darwin" ;;
  Linux/aarch64 | Linux/arm64)   triple="aarch64-unknown-linux-gnu" ;;
  Linux/x86_64 | Linux/amd64)    triple="x86_64-unknown-linux-gnu" ;;
  *) fallback "no prebuilt binary for $(uname -s)/$(uname -m)" ;;
esac

version=$(sed -n '/^\[package\]/,/^\[/{s/^version = "\(.*\)"/\1/p;}' "$repo_root/Cargo.toml" | head -n 1)
[ -n "$version" ] || fallback "could not read the version from Cargo.toml"

asset="scuttlebutt-$version-$triple.tar.gz"
tmpdir=$(mktemp -d) || fallback "could not create a temp dir"
trap 'rm -rf "$tmpdir"' EXIT

# The prebuilt is only right for the commit it was built from, and Cargo.toml
# still says the released version until the next bump lands — so a clone of main
# names a version whose binary is older than its source. herdr clones shallow and
# without tags, so `git describe` can never answer this; the release stamps its
# commit in COMMIT and we compare that against HEAD. A missing COMMIT means a
# release published before stamping, and in that window HEAD is not the tag
# either, so treating it as a mismatch is the right answer, not a gap to close.
download "$base_url/v$version/COMMIT" "$tmpdir/COMMIT" || fallback "no prebuilt binary published for v$version (no commit stamp)"
released=$(tr -d ' \t\r\n' < "$tmpdir/COMMIT")
# No git metadata means this is not a herdr install (that clones; `plugin link`
# skips [[build]]), so there is nothing to compare HEAD against.
head=$(git -C "$repo_root" rev-parse HEAD 2>/dev/null) || head=
if [ -n "$head" ] && [ -n "$released" ] && [ "$head" != "$released" ]; then
  fallback "this checkout is $(echo "$head" | cut -c1-7) but v$version was released from $(echo "$released" | cut -c1-7) — install with --ref v$version for the prebuilt binary"
fi

download "$base_url/v$version/$asset" "$tmpdir/$asset" || fallback "no prebuilt binary published for v$version ($asset)"
download "$base_url/v$version/SHA256SUMS" "$tmpdir/SHA256SUMS" || fallback "no checksums published for v$version"

# sha256sum's binary mode writes ` *name` where text mode writes `  name`; accept either.
expected=$(sed -n "s/^\([0-9a-f]\{64\}\) [ *]$asset\$/\1/p" "$tmpdir/SHA256SUMS" | head -n 1)
[ -n "$expected" ] || fallback "no checksum listed for $asset"
actual=$(sha256_of "$tmpdir/$asset") || fallback "no sha-256 tool (sha256sum/shasum) available"
[ "$actual" = "$expected" ] || fallback "checksum mismatch for $asset (expected $expected, got $actual)"

tar -xzf "$tmpdir/$asset" -C "$tmpdir" scuttlebutt || fallback "could not unpack $asset"
mkdir -p "$(dirname "$out")"
install -m 755 "$tmpdir/scuttlebutt" "$out" || fallback "could not install the binary to $out"
echo "scuttlebutt: installed prebuilt v$version ($triple), verified SHA-256."
