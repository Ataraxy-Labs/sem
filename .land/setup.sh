#!/bin/bash
# Rust stable (standalone, no rustup, so it works for any user and HOME), the packages sem's Dockerfile builds
# with, and every crate vendored to /opt/cargo-vendor so the tests build and run offline.
set -e
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends curl ca-certificates xz-utils build-essential pkg-config \
  libssl-dev cmake git
v=1.100.0
curl -fsSL https://static.rust-lang.org/dist/rust-$v-x86_64-unknown-linux-gnu.tar.xz | tar -xJ -C /tmp
/tmp/rust-$v-x86_64-unknown-linux-gnu/install.sh --prefix=/usr/local \
  --components=rustc,cargo,rust-std-x86_64-unknown-linux-gnu
rm -rf /tmp/rust-$v-x86_64-unknown-linux-gnu
export CARGO_HOME=/tmp/cargo-setup
(cd crates && cargo vendor --locked --versioned-dirs /opt/cargo-vendor > /dev/null)
rm -rf "$CARGO_HOME"
# Found from any working directory (cargo walks up to /), independent of the test user's HOME.
mkdir -p /.cargo
cat > /.cargo/config.toml <<'CFG'
[source.crates-io]
replace-with = "vendored-sources"
[source.vendored-sources]
directory = "/opt/cargo-vendor"
[net]
offline = true
CFG
chmod -R a+rX /opt/cargo-vendor /.cargo
cargo --version; rustc --version
