#!/usr/bin/env bash
# What the container runs: the same four commands the Linux job runs, in
# the same order, against the same target. Keep this in step with
# .github/workflows/ci.yml -- a local check that drifts from the remote one
# is a check that passes here and fails there.
set -euo pipefail

cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

# The keyring daemon needs a session bus, and the suite needs the daemon:
# without one the secret store falls back to a file and the backend under
# test is not the one users get. Same as CI.
dbus-run-session -- bash -c '
  echo "" | gnome-keyring-daemon --unlock --components=secrets
  cargo test --workspace --locked --target x86_64-unknown-linux-musl
  cargo test -p snob-cli --features testing --locked --target x86_64-unknown-linux-musl
'
