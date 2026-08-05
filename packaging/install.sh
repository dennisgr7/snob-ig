#!/bin/sh
#
# Installs snob on Linux or macOS.
#
#     curl -fsSL https://raw.githubusercontent.com/dennisgr7/snob-ig/main/packaging/install.sh | sh
#
# Reads three variables, all optional:
#
#     SNOB_VERSION      a version to install instead of the latest
#     SNOB_INSTALL_DIR  where to put the binary; default ~/.local/bin
#
# POSIX sh rather than bash: this is the one file that has to run before the
# user has installed anything, so it cannot assume a shell that some minimal
# containers do not ship.

set -eu

REPO="dennisgr7/snob-ig"
INSTALL_DIR="${SNOB_INSTALL_DIR:-$HOME/.local/bin}"

die() {
  echo "error: $*" >&2
  exit 1
}

# Which archive this machine needs.
target() {
  os=$(uname -s)
  arch=$(uname -m)
  case "$os/$arch" in
    Linux/x86_64) echo "x86_64-unknown-linux-gnu" ;;
    Linux/aarch64 | Linux/arm64) echo "aarch64-unknown-linux-gnu" ;;
    Darwin/arm64) echo "aarch64-apple-darwin" ;;
    # Named rather than lumped in with the unknown: an Intel Mac is a machine
    # somebody actually has, and "unsupported platform" would not tell them
    # that building from source is right there.
    Darwin/x86_64)
      die "snob has no build for Intel Macs. Build it from source instead:
    cargo install --git https://github.com/$REPO snob-cli"
      ;;
    *) die "no build for $os on $arch" ;;
  esac
}

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO "$2" "$1"
  else
    die "neither curl nor wget is installed"
  fi
}

# The checksum is not optional. This script downloads an executable over the
# network and puts it on the user's PATH; verifying it is the least it can do.
verify() {
  archive="$1" sums="$2"
  expected=$(awk -v f="$archive" '$2 == f || $2 == "*"f {print $1}' "$sums")
  [ -n "$expected" ] || die "$archive is not listed in SHA256SUMS"

  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$archive" | awk '{print $1}')
  elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$archive" | awk '{print $1}')
  else
    die "no sha256sum or shasum to check the download with"
  fi

  [ "$expected" = "$actual" ] || die "checksum mismatch for $archive"
}

main() {
  t=$(target)

  version="${SNOB_VERSION:-}"
  if [ -z "$version" ]; then
    tmp_tag=$(mktemp)
    download "https://api.github.com/repos/$REPO/releases/latest" "$tmp_tag"
    version=$(sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' "$tmp_tag" | head -1)
    rm -f "$tmp_tag"
    [ -n "$version" ] || die "could not work out the latest version"
  fi

  name="snob-v$version-$t"
  base="https://github.com/$REPO/releases/download/v$version"

  work=$(mktemp -d)
  # Whatever happens next, the download does not outlive this script.
  trap 'rm -rf "$work"' EXIT INT TERM

  echo "Downloading snob $version for $t"
  download "$base/$name.tar.gz" "$work/$name.tar.gz"
  download "$base/SHA256SUMS" "$work/SHA256SUMS"

  (cd "$work" && verify "$name.tar.gz" SHA256SUMS)
  tar -xzf "$work/$name.tar.gz" -C "$work"

  mkdir -p "$INSTALL_DIR"
  install -m 755 "$work/$name/snob" "$INSTALL_DIR/snob"

  echo "Installed $("$INSTALL_DIR/snob" --version) to $INSTALL_DIR"

  case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
      echo
      echo "$INSTALL_DIR is not on your PATH. Add this to your shell profile:"
      echo "    export PATH=\"\$PATH:$INSTALL_DIR\""
      ;;
  esac

  echo
  echo "Start with: snob login"
  echo "Before uninstalling, run \"snob purge\": the session and the database"
  echo "live outside this directory and deleting the binary will not reach them."
}

main
