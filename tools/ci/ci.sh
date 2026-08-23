#!/usr/bin/env bash
# What the container runs: the same four commands the Linux job runs, in
# the same order, against the same target. Keep this in step with
# .github/workflows/ci.yml -- a local check that drifts from the remote one
# is a check that passes here and fails there.
set -euo pipefail

# The musl target is the container's own architecture. The remote job is
# x86_64 and so is an Intel host, where this is the same string as before; on
# an ARM64 host the image is arm64, its `musl-gcc` is aarch64, and asking for
# x86_64 fails in every C build script -- aws-lc-sys, zstd-sys, libsqlite3-sys
# -- with "unrecognized command-line option '-m64'" before a test binary is
# ever linked, and a test binary of the wrong architecture could only have
# run emulated anyway. `aarch64-unknown-linux-musl` is a shipped target, and
# an ARM64 host is the only place its suite runs at all.
target="$(uname -m)-unknown-linux-musl"

cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo clippy -p snob-cli --all-targets --no-default-features --locked -- -D warnings

# The keyring daemon needs a session bus, and the suite needs the daemon:
# without one the secret store falls back to a file and the backend under
# test is not the one users get. Same as CI.
dbus-run-session -- bash -c '
  echo "" | gnome-keyring-daemon --unlock --components=secrets
  cargo test --workspace --locked --target '"$target"'
  cargo test -p snob-cli --features testing --locked --target '"$target"'
'
