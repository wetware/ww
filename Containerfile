# Multi-stage build for wetware
# Works with: podman build -t wetware:latest .
#         or: docker build -t wetware:latest -f Containerfile .

# ── Stage 1: Builder ─────────────────────────────────────────────────
FROM rust:alpine AS builder

ARG GIT_COMMIT=unknown

RUN apk add --no-cache \
    musl-dev \
    pkgconfig \
    g++ \
    make \
    cmake \
    curl \
    linux-headers

# Cap'n Proto 1.1.0 from source (must match capnpc crate version)
RUN curl -fsSL https://capnproto.org/capnproto-c++-1.1.0.tar.gz | tar xz \
    && cd capnproto-c++-1.1.0 \
    && ./configure --prefix=/usr/local \
    && make -j"$(nproc)" \
    && make install \
    && cd .. && rm -rf capnproto-c++-1.1.0

# WASM guest target
RUN rustup target add wasm32-wasip2

WORKDIR /usr/src/app

# ── Dependency cache layer ───────────────────────────────────────────
# Copy manifests, lockfile, build scripts, and capnp schemas first.
# build.rs files in membrane/ and kernel/ reference ../../capnp/*.capnp,
# so the schema dir must be present for the cache-warming build.

COPY Cargo.toml Cargo.lock build.rs ./
COPY capnp/ capnp/

# Workspace member manifests + build scripts
COPY crates/schema-id/Cargo.toml crates/schema-id/Cargo.toml
COPY crates/atom/Cargo.toml crates/atom/Cargo.toml
COPY crates/cache/Cargo.toml crates/cache/Cargo.toml
COPY crates/stem/Cargo.toml crates/stem/Cargo.toml
COPY crates/glia/Cargo.toml crates/glia/build.rs crates/glia/
COPY crates/authority/Cargo.toml crates/authority/build.rs crates/authority/
COPY crates/membrane/Cargo.toml crates/membrane/build.rs crates/membrane/
COPY crates/guest/auth/Cargo.toml crates/guest/auth/Cargo.toml
COPY std/shell/Cargo.toml std/shell/Cargo.toml
COPY std/system/Cargo.toml std/system/Cargo.toml
COPY examples/chess/Cargo.toml examples/chess/build.rs examples/chess/
COPY examples/discovery/Cargo.toml examples/discovery/build.rs examples/discovery/

# Dummy source files so cargo can resolve the workspace
RUN mkdir -p src/cli && echo 'fn main() {}' > src/cli/main.rs \
    && mkdir -p crates/schema-id/src && echo '' > crates/schema-id/src/lib.rs \
    && mkdir -p crates/atom/src && echo '' > crates/atom/src/lib.rs \
    && mkdir -p crates/cache/src && echo '' > crates/cache/src/lib.rs \
    && mkdir -p crates/stem/src && echo '' > crates/stem/src/lib.rs \
    && mkdir -p crates/glia/src && echo '' > crates/glia/src/lib.rs \
    && mkdir -p crates/authority/src && echo '' > crates/authority/src/lib.rs \
    && mkdir -p crates/membrane/src && echo '' > crates/membrane/src/lib.rs \
    && mkdir -p crates/guest/auth/src && echo '' > crates/guest/auth/src/lib.rs \
    && mkdir -p std/shell/src && echo 'fn main() {}' > std/shell/src/main.rs \
    && mkdir -p std/system/src && echo '' > std/system/src/lib.rs \
    && mkdir -p examples/chess/src && echo '' > examples/chess/src/lib.rs \
    && mkdir -p examples/discovery/src && echo '' > examples/discovery/src/lib.rs

# Warm the dependency cache (errors expected from dummy sources; || true)
RUN cargo build --release || true
RUN cargo build --release --target wasm32-wasip2 || true

# ── Full source build ────────────────────────────────────────────────
# Remove dummy sources, copy real project
RUN find . -name '*.rs' -path '*/src/*' -delete
COPY . .

# GIT_COMMIT set after cache-warming so the hash doesn't bust dep cache.
ENV GIT_COMMIT=${GIT_COMMIT}

# Build std + echo example (embedded by build.rs), then host binary
RUN make std echo host

# ── Stage 2: Runtime ─────────────────────────────────────────────────
FROM gcr.io/distroless/static-debian12

COPY --from=builder /usr/src/app/target/release/ww /usr/local/bin/ww

# Kernel layer (FHS: bin/main.wasm)
COPY --from=builder /usr/src/app/std/kernel/bin/main.wasm \
     /usr/share/wetware/kernel/bin/main.wasm

# Shell layer (WASM + schema + init.d)
COPY --from=builder /usr/src/app/std/shell/bin/shell.wasm \
     /usr/share/wetware/shell/bin/shell.wasm
COPY --from=builder /usr/src/app/std/shell/bin/shell.capnpc \
     /usr/share/wetware/shell/bin/shell.capnpc
COPY --from=builder /usr/src/app/std/shell/etc/init.d/50-shell.glia \
     /usr/share/wetware/shell/etc/init.d/50-shell.glia

USER 1000:1000
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/ww"]
CMD ["run", "/usr/share/wetware/kernel", "/usr/share/wetware/shell"]
