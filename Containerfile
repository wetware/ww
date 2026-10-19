# Multi-stage build for wetware
# Works with: podman build -t wetware:latest .
#         or: docker build -t wetware:latest -f Containerfile .
# Requires Docker BuildKit (Dockerfile frontend 1.2+) or Podman 4.1+.
# The pinned WASI tools below require a linux/amd64 builder.

# ── Stage 1: Builder ─────────────────────────────────────────────────
FROM rust:bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        cmake \
        curl \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Cap'n Proto 1.1.0 from source (must match capnpc crate version)
RUN curl -fsSL https://capnproto.org/capnproto-c++-1.1.0.tar.gz | tar xz \
    && cd capnproto-c++-1.1.0 \
    && ./configure --prefix=/usr/local \
    && make -j"$(nproc)" \
    && make install \
    && cd .. && rm -rf capnproto-c++-1.1.0

# Native WASI P3 toolchain. Keep these versions and checksums synchronized with
# scripts/build_wasip3_component.sh and .github/workflows/rust.yml.
RUN rustup toolchain install nightly-2026-08-30 --component rust-src \
    && mkdir -p /opt/p3-tools \
    && curl -fL \
      https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-34/wasi-sdk-34.0-x86_64-linux.tar.gz \
      -o /tmp/wasi-sdk.tar.gz \
    && echo "b761e3a0721dbae9c09a0059e5fdb2bf917d1b4a8a7b430fb3b5aafb0984b2c4  /tmp/wasi-sdk.tar.gz" \
      | sha256sum --check --strict \
    && tar -xzf /tmp/wasi-sdk.tar.gz -C /opt/p3-tools \
    && curl -fL \
      https://github.com/bytecodealliance/wasm-tools/releases/download/v1.258.0/wasm-tools-1.258.0-x86_64-linux.tar.gz \
      -o /tmp/wasm-tools.tar.gz \
    && echo "b52d14eb74a4852cc249369bd4480c2b2fdd876145f41db51ff52269ded240ce  /tmp/wasm-tools.tar.gz" \
      | sha256sum --check --strict \
    && tar -xzf /tmp/wasm-tools.tar.gz -C /opt/p3-tools \
    && rm /tmp/wasi-sdk.tar.gz /tmp/wasm-tools.tar.gz

ENV WASI_SDK_PATH=/opt/p3-tools/wasi-sdk-34.0-x86_64-linux
ENV WASM_TOOLS=/opt/p3-tools/wasm-tools-1.258.0-x86_64-linux/wasm-tools

WORKDIR /usr/src/app

# Resolve the host toolchain before source changes invalidate the build layer.
COPY rust-toolchain.toml ./
RUN rustup show active-toolchain

# ── Full source build ────────────────────────────────────────────────
COPY . .

# Declare provenance after toolchain installation so SHA changes reuse setup.
ARG WW_BUILD_GIT_SHA=unknown
ENV WW_BUILD_GIT_SHA=${WW_BUILD_GIT_SHA}

# Cache the real host and standalone P3 builds, including target/wasip3.
# The locked target mount serializes builds sharing these caches. Cargo tracks
# manifests, lockfiles, build inputs, and compiler used by each build.
# Refresh source timestamps on a layer miss: an older checkout can otherwise
# appear unchanged to Cargo when paired with newer cached target fingerprints.
# This rebuilds local crates while retaining compiled registry/git dependencies.
# P3 outputs are copied to std/ and examples/ by the build script. The host
# binary must leave the target cache before this RUN ends for the runtime COPY.
# Keep the sole locked mount last for Podman 4.1 / Buildah 1.26 compatibility.
RUN --mount=type=cache,id=wetware-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=wetware-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=wetware-target,target=/usr/src/app/target,sharing=locked \
    find . -path ./target -prune -o -type f -exec touch {} + \
    && make std echo host \
    && mkdir -p /out \
    && cp target/release/ww /out/ww

# ── Stage 2: Runtime ─────────────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12

COPY --from=builder /out/ww /usr/local/bin/ww

# Kernel layer (FHS: bin/main.wasm)
COPY --from=builder /usr/src/app/std/kernel/bin/main.wasm \
     /usr/share/wetware/kernel/bin/main.wasm

USER 1000:1000
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/ww"]
CMD ["run", "/usr/share/wetware/kernel"]
