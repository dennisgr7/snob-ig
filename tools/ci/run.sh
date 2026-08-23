#!/usr/bin/env bash
# Run the Linux CI job locally. From the repository root:
#
#     bash tools/ci/run.sh
#
# The first run builds the image and compiles everything from cold, which
# takes a few minutes; the two named volumes keep the registry and the
# target directory between runs, so a second run is under a minute.
#
# CARGO_BUILD_JOBS is bounded by the memory of the VM, because linking a
# dozen test binaries at once is what exhausts the 8 GB Docker Desktop gives
# its VM by default, and a linker killed for memory reads as a broken build
# rather than a full one. One job per 2 GB, never more than the VM's CPUs:
# measured, the container peaks at 5.4 GB with four jobs and 9.4 GB with
# eight, so the default VM stays at four and a 16 GB one uses every core,
# for about thirty seconds off a cold run. A CARGO_BUILD_JOBS already in the
# environment still wins.
set -euo pipefail
cd "$(dirname "$0")/../.."

# Git Bash on Windows hands `pwd` out as /c/Users/..., which Docker Desktop
# does not read as a Windows path; `pwd -W` is the spelling it does. The
# variable stops the same shell rewriting the container-side paths below.
root="$(pwd -W 2>/dev/null || pwd)"
export MSYS_NO_PATHCONV=1

# One `docker info`, two fields: the client takes a moment to start.
read -r mem_bytes cpus < <(docker info --format '{{.MemTotal}} {{.NCPU}}')
jobs=$(( mem_bytes / (2 * 1024 * 1024 * 1024) ))
(( jobs < 1 )) && jobs=1
(( jobs > cpus )) && jobs=$cpus

docker build -q -t snob-ci tools/ci >/dev/null
exec docker run --rm \
    -e CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-$jobs}" \
    -v "$root:/src" \
    -v snob-ci-cargo:/cargo \
    -v snob-ci-target:/target \
    snob-ci
