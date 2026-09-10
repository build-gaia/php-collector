ARG PHP_VERSION=8.2
ARG RUST_VERSION=1.88.0

# WHY THE TOOLCHAIN IS COPIED, NOT INSTALLED.
# This used to run `curl https://sh.rustup.rs | sh` in every builder. rustup
# resolves and downloads independently per image, so each PHP version grew its
# OWN ~1.5 GB toolchain layer — 12 builders meant 12 copies of the same thing,
# and that duplication was most of the ~40 GB the builders occupied.
#
# COPY --from a pinned official image emits a layer whose content is identical
# for every PHP version, so Docker content-addresses it to ONE blob that all
# glibc builders share. It also pins the toolchain, which `--default-toolchain
# stable` never did: a rebuild months apart silently changed compilers.
FROM rust:${RUST_VERSION}-bookworm AS toolchain

FROM php:${PHP_VERSION}-cli-bookworm

# llvm stays: docker-build-extensions.sh calls `llvm-config --libdir` to locate
# libclang, and falls back to a WRONG /usr/lib when it is missing.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config libclang-dev clang llvm \
    && rm -rf /var/lib/apt/lists/*

COPY --from=toolchain /usr/local/rustup /usr/local/rustup
COPY --from=toolchain /usr/local/cargo  /usr/local/cargo

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
# bindgen needs an explicit libclang on bookworm (libclang.so lives under llvm-*).
ENV LIBCLANG_PATH=/usr/lib/llvm-14/lib

# `bash -lc` (which the build scripts use) re-sources /etc/profile, whose FIRST
# line hard-overwrites PATH and drops /usr/local/cargo/bin. The old rustup install
# survived that only because rustup appends `. "$HOME/.cargo/env"` to ~/.profile;
# copying the toolchain removes that mechanism, so cargo must be put back on PATH
# explicitly. Symlinks into /usr/local/bin hold even for a non-login shell.
RUN printf '%s\n' \
      'export RUSTUP_HOME=/usr/local/rustup' \
      'export CARGO_HOME=/usr/local/cargo' \
      'export PATH=/usr/local/cargo/bin:$PATH' > /etc/profile.d/rust.sh \
    && chmod +x /etc/profile.d/rust.sh \
    && ln -sf /usr/local/cargo/bin/* /usr/local/bin/

WORKDIR /build
