ARG PHP_VERSION=8.2
FROM php:${PHP_VERSION}-cli-bookworm

RUN apt-get update && apt-get install -y --no-install-recommends \
    curl build-essential pkg-config libclang-dev clang llvm \
    && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
ENV PATH="/root/.cargo/bin:${PATH}"
# bindgen needs an explicit libclang on bookworm (libclang.so lives under llvm-*).
ENV LIBCLANG_PATH=/usr/lib/llvm-14/lib

WORKDIR /build
