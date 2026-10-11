FROM rust@sha256:0cce0a5e0e8ba67b455257a3a02a1d99005f382748789d6464460028810f1627 AS build
ARG TARGETARCH
ENV CARGO_HOME=/cargo CARGO_TARGET_DIR=/cargo-target
WORKDIR /work
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY assets assets
COPY vendor vendor
RUN --mount=type=cache,id=dockstride-registry,target=/cargo/registry,sharing=locked \
    --mount=type=cache,id=dockstride-git,target=/cargo/git,sharing=locked \
    --mount=type=cache,id=dockstride-target-${TARGETARCH},target=/cargo-target,sharing=locked \
    (cargo build --locked --offline --release --bin dks || cargo build --locked --release --bin dks) \
    && install -D /cargo-target/release/dks /out/dks \
    && /out/dks --version && sha256sum /out/dks
FROM scratch
COPY --from=build /out/dks /dks
