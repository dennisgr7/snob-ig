# The Linux job of .github/workflows/ci.yml, runnable on this machine.
#
# It is the one job of the three that differs most from a developer's host:
# a musl target, a real Secret Service keyring behind a session bus, and a
# glibc-free link. Waiting on a remote runner to learn that a change broke
# one of those costs ten minutes a round; this image answers in under a
# minute once its caches are warm. The Windows and macOS jobs are not here,
# because a container cannot run either -- the host is the Windows job.
#
# The image pins the same compiler rust-toolchain.toml pins, so what is
# tested here is what ships. Bump the two together.
FROM rust:1.97.1-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
        musl-tools gnome-keyring dbus-x11 libdbus-1-3 file \
    && rm -rf /var/lib/apt/lists/*
# The musl target of whichever architecture this image is built for: x86_64
# on an Intel host, aarch64 on an ARM64 one. ci.sh picks the same string the
# same way, and `musl-tools` only ever installs the host's own musl-gcc.
RUN rustup target add "$(uname -m)-unknown-linux-musl" \
    && rustup component add rustfmt clippy

ENV CARGO_TERM_COLOR=always \
    CARGO_INCREMENTAL=0 \
    CARGO_HOME=/cargo \
    CARGO_TARGET_DIR=/target
WORKDIR /src
COPY ci.sh /ci.sh
ENTRYPOINT ["bash", "/ci.sh"]
