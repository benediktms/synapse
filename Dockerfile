FROM rust:1.96-trixie AS build

WORKDIR /app
ENV SQLX_OFFLINE=1 \
    HF_HOME=/model \
    FASTEMBED_CACHE_DIR=/model

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY xtask ./xtask

RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/app/target,sharing=locked \
    cargo build --release --locked --bin synapse-server \
    && install -Dm755 target/release/synapse-server /out/synapse-server \
    && ldd /out/synapse-server

# Warm-up embed against an empty registry: loads the model (downloading it once,
# here) so the runtime stage ships the cache and never reaches the HF hub.
RUN mkdir -p /warm \
    && SYNAPSE_DATA_DIR=/warm /out/synapse-server reembed --model bge-small-en-v1.5 \
    && rm -rf /warm \
    && du -sh /model

# glibc, not musl: ONNX Runtime ships no musl build. Trixie rather than bookworm
# because ort's prebuilt static ONNX Runtime needs a libstdc++ from GCC 13 or newer.
FROM debian:trixie-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends libssl3 libstdc++6 \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --system --uid 10001 --user-group --home-dir /data synapse \
    && install -d -o synapse -g synapse -m 700 /data

COPY --from=build /model /opt/synapse/model
COPY --from=build /out/synapse-server /usr/local/bin/synapse-server

# HF_HOME wins over FASTEMBED_CACHE_DIR in fastembed's cache resolution; both point
# at the baked cache so an upstream precedence change cannot trigger a download.
ENV HF_HOME=/opt/synapse/model \
    FASTEMBED_CACHE_DIR=/opt/synapse/model \
    SYNAPSE_DATA_DIR=/data \
    SYNAPSE_BIND=0.0.0.0:8737 \
    SYNAPSE_ALLOW_NONLOCAL=1

USER synapse
WORKDIR /data
EXPOSE 8737
ENTRYPOINT ["synapse-server"]
CMD ["serve"]
