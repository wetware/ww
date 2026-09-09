# Multi-stage build for wetware
# Works with: podman build -t wetware:latest .
#         or: docker build -t wetware:latest -f Containerfile .

# ── Stage 1: Builder ─────────────────────────────────────────────────
FROM rust:bookworm AS builder

ARG WW_BUILD_GIT_SHA=unknown

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

# ── Dependency cache layer ───────────────────────────────────────────
# Copy manifests, lockfile, build scripts, and capnp schemas first.
# build.rs files in membrane/ and kernel/ reference ../../capnp/*.capnp,
# so the schema dir must be present for the cache-warming build.

COPY Cargo.toml Cargo.lock build.rs ./
COPY capnp/ capnp/

# Workspace member manifests + build scripts
COPY crates/atom/Cargo.toml crates/atom/Cargo.toml
COPY crates/cache/Cargo.toml crates/cache/Cargo.toml
COPY crates/stem/Cargo.toml crates/stem/Cargo.toml
COPY crates/authority/Cargo.toml crates/authority/build.rs crates/authority/
COPY crates/membrane/Cargo.toml crates/membrane/build.rs crates/membrane/
COPY crates/guest/auth/Cargo.toml crates/guest/auth/Cargo.toml
COPY std/system/Cargo.toml std/system/Cargo.toml
COPY examples/chess/Cargo.toml examples/chess/build.rs examples/chess/
COPY examples/discovery/Cargo.toml examples/discovery/build.rs examples/discovery/

# Dummy source files so cargo can resolve the workspace
RUN mkdir -p src/cli && echo 'fn main() {}' > src/cli/main.rs \
    && mkdir -p crates/atom/src && echo '' > crates/atom/src/lib.rs \
    && mkdir -p crates/cache/src && echo '' > crates/cache/src/lib.rs \
    && mkdir -p crates/stem/src && echo '' > crates/stem/src/lib.rs \
    && mkdir -p crates/authority/src && echo '' > crates/authority/src/lib.rs \
    && mkdir -p crates/membrane/src && echo '' > crates/membrane/src/lib.rs \
    && mkdir -p crates/guest/auth/src && echo '' > crates/guest/auth/src/lib.rs \
    && mkdir -p std/system/src && echo '' > std/system/src/lib.rs \
    && mkdir -p examples/chess/src && echo '' > examples/chess/src/lib.rs \
    && mkdir -p examples/discovery/src && echo '' > examples/discovery/src/lib.rs

# Warm the dependency cache (errors expected from dummy sources; || true)
RUN cargo build --release || true

# ── Full source build ────────────────────────────────────────────────
# Remove dummy sources, copy real project
RUN find . -name '*.rs' -path '*/src/*' -delete
COPY . .

# WW_BUILD_GIT_SHA is set after cache-warming so the hash does not bust the dependency cache.
ENV WW_BUILD_GIT_SHA=${WW_BUILD_GIT_SHA}

# Build std + echo example (embedded by build.rs), then host binary
RUN make std echo host

# ── Stage 2: Runtime ─────────────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12

COPY --from=builder /usr/src/app/target/release/ww /usr/local/bin/ww

# Kernel layer (FHS: bin/main.wasm)
COPY --from=builder /usr/src/app/std/kernel/bin/main.wasm \
     /usr/share/wetware/kernel/bin/main.wasm

USER 1000:1000
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/ww"]
CMD ["run", "/usr/share/wetware/kernel"]
