# Reproducible SBF build environment for quotebook-program (docs/PLAN-improvements.md, I2).
#
# The same pins as CI (.github/workflows/ci.yml): Agave CLI 4.4.0 (cargo-build-sbf 4.5.0),
# platform-tools v1.57 (SBF Rust 1.95.0), host Rust 1.99.1. No solana-verifiable-build image
# exists for Agave 4.4 (Docker Hub, checked 2026-11-02: newest tag 4.3.0), so this image is
# built from the verified release archives instead. The base image is pinned by digest; the
# Ubuntu packages are not, and do not reach the program binary (only the Rust and SBF
# toolchains do).
FROM ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852

ARG RUST_PIN=1.99.1
ARG AGAVE_VERSION=v4.4.0
ARG AGAVE_SHA256=3363f4728fe786ac7036e342a20df928aa2e3e43f807fbdce67814840538c574
ARG PLATFORM_TOOLS_VERSION=v1.57
ARG PLATFORM_TOOLS_SHA256=b0f7af104adf726fff2a6a09ea2eb2f2d2965c92295f4d7388c08d140e0c2b00

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl bzip2 build-essential \
 && rm -rf /var/lib/apt/lists/*

# RUSTUP_TOOLCHAIN overrides the repository's rust-toolchain.toml (`channel = "stable"`), so
# the archived tree cannot switch the host toolchain away from the pin (Codex, I2 review).
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:/opt/agave/solana-release/bin:$PATH \
    RUSTUP_TOOLCHAIN=$RUST_PIN RUSTUP_AUTO_INSTALL=0
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain "$RUST_PIN"

RUN set -eu; base=https://github.com/anza-xyz; cd /tmp \
 && curl -fsSL -o agave.tar.bz2 \
      "$base/agave/releases/download/$AGAVE_VERSION/solana-release-x86_64-unknown-linux-gnu.tar.bz2" \
 && curl -fsSL -o platform-tools.tar.bz2 \
      "$base/platform-tools/releases/download/$PLATFORM_TOOLS_VERSION/platform-tools-linux-x86_64.tar.bz2" \
 && echo "$AGAVE_SHA256  agave.tar.bz2" | sha256sum -c - \
 && echo "$PLATFORM_TOOLS_SHA256  platform-tools.tar.bz2" | sha256sum -c - \
 && mkdir -p /opt/agave && tar -xjf agave.tar.bz2 -C /opt/agave \
 && dest=/root/.cache/solana/$PLATFORM_TOOLS_VERSION/platform-tools \
 && mkdir -p "$dest" && tar -xjf platform-tools.tar.bz2 -C "$dest" \
 && rm agave.tar.bz2 platform-tools.tar.bz2

# Fails the image build unless every tool reports its pin (as the CI action does).
RUN set -eu; sbf=$(cargo build-sbf --version); echo "$sbf" \
 && echo "$sbf" | grep -qF "cargo-build-sbf 4.5.0" \
 && echo "$sbf" | grep -qF "platform-tools: v1.57" \
 && echo "$sbf" | grep -qF "Rust: 1.95.0" \
 && rustc --version | grep -qF "rustc $RUST_PIN"

WORKDIR /work
