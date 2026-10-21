#!/bin/sh
# Build the Linux release of rune for every Linux, not only the one it is
# built on:
#
#   scripts/build_linux_release.sh [gnu|musl]
#
#   gnu   (default) linked against glibc 2.17 — the oldest glibc Rust itself
#         supports — inside the manylinux2014 image (CentOS 7 userland), so
#         it runs on every glibc distribution since 2012: Debian 8+,
#         Ubuntu 14.04+, RHEL/CentOS 7+, and any newer one.
#   musl  one static file needing no C library at all — Alpine and any other
#         Linux without glibc.
#
# A binary built on this machine instead asks for the glibc of this machine
# (2.38/2.39 on Debian 13: `__isoc23_*` from SQLite's C, `pidfd_*` from Rust's
# std) and refuses to start anywhere older. The image's compiler and C library
# decide that, so the build runs inside one; the result is checked afterwards
# (scripts/check_linux_binary.py), never trusted.
#
# Builds for the machine's own architecture (x86_64 or aarch64); CI runs it
# once per architecture. Needs Docker. Output:
#   target/linux-release/<libc>-<arch>/release/rune
set -eu
libc=${1:-gnu}
root=$(cd "$(dirname "$0")/.." && pwd)
arch=$(uname -m)
case "$arch" in
  x86_64|aarch64) ;;
  arm64) arch=aarch64 ;;
  *) echo "build_linux_release: unsupported architecture $arch" >&2; exit 2 ;;
esac

# Images pinned by digest: what the release is built with never changes
# unless this file does.
case "$libc-$arch" in
  gnu-x86_64) image=quay.io/pypa/manylinux2014_x86_64@sha256:d6b5342c73e51b8eacd114cb41733798b749494d0a749cef0bd76c99fb47d5c6 ;;
  gnu-aarch64) image=quay.io/pypa/manylinux2014_aarch64@sha256:d41ea3e9c9ad7a47e393673d0e8acba58fa411ceff58bc6b88bca38795ec938e ;;
  musl-*) image=alpine:3.22@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8 ;;
  *) echo "usage: build_linux_release.sh [gnu|musl]" >&2; exit 2 ;;
esac
case "$libc" in
  gnu) triple=$arch-unknown-linux-gnu ;;
  musl) triple=$arch-unknown-linux-musl ;;
esac

# Everything the build keeps lives under target/ (ignored by Git): the
# toolchain and crate caches, so a second build installs nothing again.
out="target/linux-release/$libc-$arch"
cache="target/linux-release/cache-$libc-$arch"
mkdir -p "$root/$out" "$root/$cache"

# Alpine brings no compiler; manylinux2014 brings GCC 10 (devtoolset) and its
# own C library already.
if [ "$libc" = musl ]; then
  prepare='apk add --no-cache build-base curl perl linux-headers >/dev/null'
else
  prepare=':'
fi

docker run --rm \
  -v "$root:/work" -w /work \
  -e HOME="/work/$cache" \
  -e CARGO_HOME="/work/$cache/cargo" \
  -e RUSTUP_HOME="/work/$cache/rustup" \
  -e CARGO_TARGET_DIR="/work/$out" \
  -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  "$image" sh -euc "
    $prepare
    if [ ! -x \"\$CARGO_HOME/bin/cargo\" ]; then
      curl -sSf --proto '=https' --tlsv1.2 https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal --default-toolchain none
    fi
    export PATH=\"\$CARGO_HOME/bin:\$PATH\"
    # rust-toolchain.toml names the toolchain: installed as it says, then
    # the target beside it.
    rustup toolchain install >/dev/null
    rustup target add $triple >/dev/null
    cargo build --release --locked -p runecli --target $triple
    mkdir -p /work/$out/release
    cp /work/$out/$triple/release/rune /work/$out/release/rune
    chown -R \"\$HOST_UID:\$HOST_GID\" /work/$out /work/$cache
  "

python3 -I "$root/scripts/check_linux_binary.py" "$root/$out/release/rune" --libc "$libc"
echo "$out/release/rune"
