#!/usr/bin/env bash
# Run the Linux CI job locally. From the repository root:
#
#     bash ci/local/run.sh
#
# The first run builds the image and compiles everything from cold, which
# takes a few minutes; the two named volumes keep the registry and the
# target directory between runs, so a second run is under a minute.
#
# CARGO_BUILD_JOBS is capped because linking a dozen test binaries at once
# is what exhausts the 8 GB Docker Desktop gives its VM by default, and a
# linker killed for memory reads as a broken build rather than a full one.
set -euo pipefail
cd "$(dirname "$0")/../.."

# Git Bash on Windows hands `pwd` out as /c/Users/..., which Docker Desktop
# does not read as a Windows path; `pwd -W` is the spelling it does. The
# variable stops the same shell rewriting the container-side paths below.
root="$(pwd -W 2>/dev/null || pwd)"
export MSYS_NO_PATHCONV=1

docker build -q -t snob-ci ci/local >/dev/null
exec docker run --rm \
    -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" \
    -v "$root:/src" \
    -v snob-ci-cargo:/cargo \
    -v snob-ci-target:/target \
    snob-ci
