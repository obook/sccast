#!/bin/sh
#
# Name: build-deb.sh
# Description: Build the sccast .deb for one target inside an Ubuntu container.
# Usage: packaging/build-deb.sh neon|ubuntu26.04
# Output: target/debian/sccast_<version>-1~<target>_amd64.deb
# Author: O. Booklage
# Date: October 2026
# Licence: MIT
#
# POSIX sh has no pipefail; the script has no pipe.
set -eu

case "${1:-}" in
    # KDE neon is built on Ubuntu 24.04: a package built there installs on neon.
    neon) release=24.04 ;;
    ubuntu26.04) release=26.04 ;;
    *) echo "usage: $0 neon|ubuntu26.04" >&2; exit 2 ;;
esac
target=$1

# Docker is the only host dependency: Rust and cargo-deb live in the image.
if ! command -v docker >/dev/null 2>&1; then
    echo "Docker is missing. Install it with:" >&2
    echo "  sudo apt install docker.io && sudo usermod -aG docker \$USER" >&2
    echo "then log out and back in." >&2
    exit 1
fi
if ! docker info >/dev/null 2>&1; then
    echo "Cannot reach the Docker daemon. Start it and allow your user with:" >&2
    echo "  sudo systemctl enable --now docker && sudo usermod -aG docker \$USER" >&2
    echo "then log out and back in." >&2
    exit 1
fi

image="sccast-build:$release"
cd "$(dirname "$0")/.."

# Rust from rustup: the distribution's rustc can be older than the crates require.
# Toolchain in /usr/local, writable, so the build runs as the calling user (files in target/ stay yours).
# Host network: no veth interface per step, so NetworkManager stops notifying each one.
docker build --network host -t "$image" --build-arg RELEASE="$release" - <<'EOF'
ARG RELEASE
FROM ubuntu:${RELEASE}
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        ca-certificates curl build-essential pkg-config dpkg-dev libgstreamer1.0-dev \
    && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
    && cargo install cargo-deb --locked \
    && chmod -R a+rwX /usr/local/cargo /usr/local/rustup
EOF

# One target dir per release: binaries built against different libraries must not mix.
docker run --rm --network host --user "$(id -u):$(id -g)" -e HOME=/tmp \
    -e CARGO_TARGET_DIR="/src/target/$target" \
    -v "$PWD:/src" -w /src "$image" \
    sh -c "cargo deb --deb-revision '1~$target' -o target/debian/"
