# Multi-stage build for wetware
# Works with: podman build -t wetware:latest .
#         or: docker build -t wetware:latest -f Containerfile .
# Requires Docker BuildKit (Dockerfile frontend 1.2+) or Podman 4.1+.
# First run `make fhs` to produce the validated kernel/status inputs.
# This recipe builds the Host and consumes those exact canonical producer bytes.

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

WORKDIR /usr/src/app

# Resolve the host toolchain before source changes invalidate the build layer.
COPY rust-toolchain.toml ./
RUN rustup show active-toolchain

# ── Full source build ────────────────────────────────────────────────
COPY . .

# Declare provenance after toolchain installation so SHA changes reuse setup.
ARG WW_BUILD_GIT_SHA=unknown
ENV WW_BUILD_GIT_SHA=${WW_BUILD_GIT_SHA}

# Reuse native Cargo dependencies. Stage from the canonical inputs in this
# context; rebuilding P3 here would create a second independent package producer.
# Refresh local source timestamps when the source layer changes.
RUN --mount=type=cache,id=wetware-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=wetware-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=wetware-target,target=/usr/src/app/target,sharing=locked \
    find . -path ./target -prune -o -type f -exec touch {} + \
    && mkdir -p /out \
    && cargo run --locked --example stage_fhs -- stage \
       std/kernel/bin/main.wasm std/status/bin/status.wasm /out/fhs \
    && cargo build --release --locked \
    && cp target/release/ww /out/ww \
    && mkdir -p /out/home/.ww \
    && chown -R 1000:1000 /out/home

# ── Stage 2: Runtime ─────────────────────────────────────────────────
FROM gcr.io/distroless/cc-debian12

COPY --from=builder /out/ww /usr/local/bin/ww

# Canonical immutable FHS stays outside the writable HOME volume.
COPY --from=builder /out/fhs/ /usr/share/wetware/kernel/
COPY --from=builder --chown=1000:1000 /out/home/ /home/ww/
ENV HOME=/home/ww
ENV WW_FHS_ROOT=/usr/share/wetware/kernel
WORKDIR /home/ww
VOLUME ["/home/ww"]

USER 1000:1000
EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/ww"]
CMD ["run"]
