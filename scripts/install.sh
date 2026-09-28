#!/bin/sh
# Install the `cargo sekvent` CLI.
#
#   curl -fsSL https://raw.githubusercontent.com/westito/sekvent/master/scripts/install.sh | sh
#
# Downloads the prebuilt binary of the rolling `cli-latest` release for this
# platform, checks its SHA-256 and installs it into
# ${SEKVENT_INSTALL_DIR:-${CARGO_HOME:-$HOME/.cargo}/bin}. When there is no
# prebuilt binary for the platform, or the download fails, it builds the CLI
# from source with `cargo install` instead, pinned to the same `cli-latest`
# tag the release was built from (never the moving branch head). A checksum
# mismatch is always an error, never a reason to fall back.
#
# Environment:
#   SEKVENT_INSTALL_DIR  directory to install into
#   CARGO_HOME           used for the default directory (default: ~/.cargo)
set -eu

repo_url="https://github.com/westito/sekvent"
release_tag="cli-latest"
binary="cargo-sekvent"

say() {
  printf 'sekvent-install: %s\n' "$*" >&2
}

die() {
  say "error: $*"
  exit 1
}

have() {
  command -v "$1" >/dev/null 2>&1
}

install_dir="${SEKVENT_INSTALL_DIR:-${CARGO_HOME:-$HOME/.cargo}/bin}"

work="$(mktemp -d 2>/dev/null || mktemp -d -t sekvent-install)"
cleanup() {
  rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# The target triple of a prebuilt asset for this machine, or nothing.
detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$arch" in
    x86_64 | amd64) arch="x86_64" ;;
    aarch64 | arm64) arch="aarch64" ;;
    *) return 0 ;;
  esac
  case "$os" in
    Linux)
      # Only glibc builds are published.
      if have ldd && ldd --version 2>&1 | grep -qi musl; then
        return 0
      fi
      printf '%s-unknown-linux-gnu\n' "$arch"
      ;;
    Darwin)
      # A shell under Rosetta reports x86_64 on Apple silicon; prefer the
      # native build.
      if [ "$arch" = "x86_64" ] &&
        [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
        arch="aarch64"
      fi
      printf '%s-apple-darwin\n' "$arch"
      ;;
  esac
}

download() {
  curl --fail --silent --show-error --location --retry 2 --output "$2" "$1"
}

sha256_of() {
  if have sha256sum; then
    sha256sum "$1" | awk '{ print tolower($1) }'
  else
    shasum -a 256 "$1" | awk '{ print tolower($1) }'
  fi
}

# The lower-case hex digest of a .sha256 file (`<hex>` or `<hex>  <name>`).
read_checksum() {
  digest="$(awk 'NF { print $1; exit }' "$1" | tr 'A-F' 'a-f')"
  case "$digest" in
    '' | *[!0-9a-f]*) return 1 ;;
  esac
  [ "${#digest}" -eq 64 ] || return 1
  printf '%s\n' "$digest"
}

# Copy $1 to $install_dir/$binary via a temporary file in the same directory,
# so the final rename is atomic and a failure never leaves half a binary.
place_binary() {
  mkdir -p "$install_dir" || die "cannot create $install_dir"
  staged="$install_dir/.$binary.install.$$"
  cp "$1" "$staged" || {
    rm -f "$staged"
    die "cannot write into $install_dir"
  }
  chmod 755 "$staged"
  mv -f "$staged" "$install_dir/$binary" || {
    rm -f "$staged"
    die "cannot replace $install_dir/$binary"
  }
}

# Download, verify and unpack the prebuilt asset for $1. Returns non-zero
# when the asset cannot be downloaded; exits on a checksum mismatch.
install_prebuilt() {
  asset="$binary-$1.tar.gz"
  url="$repo_url/releases/download/$release_tag/$asset"
  say "downloading $url"
  download "$url" "$work/$asset" || return 1
  download "$url.sha256" "$work/$asset.sha256" || return 1

  expected="$(read_checksum "$work/$asset.sha256")" ||
    die "$asset.sha256 holds no SHA-256 digest"
  actual="$(sha256_of "$work/$asset")"
  if [ "$actual" != "$expected" ]; then
    die "checksum mismatch for $asset: expected $expected, got $actual"
  fi
  say "checksum ok ($expected)"

  mkdir -p "$work/unpacked"
  tar -xzf "$work/$asset" -C "$work/unpacked" || die "cannot unpack $asset"
  found="$(find "$work/unpacked" -type f -name "$binary" | head -n 1)"
  [ -n "$found" ] || die "$asset holds no $binary binary"
  place_binary "$found"
  say "installed the $release_tag build for $1"
}

install_from_source() {
  have cargo || die "no prebuilt $binary for this platform and cargo is not installed; install Rust from https://rustup.rs and run this script again"
  say "building $binary from source at tag $release_tag with cargo install (this takes a few minutes)"
  cargo install --locked --root "$work/cargo-root" --git "$repo_url" --tag "$release_tag" "$binary" ||
    die "cargo install of $repo_url at tag $release_tag failed"
  place_binary "$work/cargo-root/bin/$binary"
  say "installed a source build of $repo_url at tag $release_tag"
}

have curl || die "curl is required"
have tar || die "tar is required"
have sha256sum || have shasum || die "sha256sum or shasum is required"
target="$(detect_target)"
if [ -z "$target" ]; then
  say "no prebuilt $binary for $(uname -s) $(uname -m)"
  install_from_source
elif ! install_prebuilt "$target"; then
  say "the prebuilt $binary for $target could not be downloaded"
  install_from_source
fi

say "$binary is at $install_dir/$binary"
case ":$PATH:" in
  *":$install_dir:"*)
    say "run \`cargo sekvent --help\` to get started"
    ;;
  *)
    say "note: $install_dir is not on PATH; add it to use \`cargo sekvent\`"
    ;;
esac
