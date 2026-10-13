FROM rust:1.95.0-slim-bookworm@sha256:d7482085ff5b415f84dba5647ae71606650bdef00db7aeb69f4b3d170c3e4082

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        cmake \
        git \
        libssl-dev \
        pkg-config \
        python3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
