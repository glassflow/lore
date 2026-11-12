# Builder and runtime both use Debian trixie. fastembed's ONNX runtime needs the
# libstdc++ from GCC 13+, and bookworm fails to link (spike on 2026-10-08).
FROM rust:1.95-trixie AS builder
ARG VERSION=dev
WORKDIR /src

# Cache dependencies: build a stub crate first.
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
    && cargo build --release --locked

# Build the real sources. Later tickets embed files from migrations/ and tests/fixtures/.
COPY src ./src
COPY migrations ./migrations
COPY tests/fixtures ./tests/fixtures
# COPY keeps the old mtimes, so without this cargo would reuse the stub build.
# LORE_VERSION is set here only, so the dependency cache above does not depend on it.
RUN touch src/main.rs src/lib.rs && LORE_VERSION=$VERSION cargo build --release --locked

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git libssl3t64 \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --uid 10001 --create-home --home-dir /home/lore lore \
    && mkdir -p /var/lib/lore && chown lore:lore /var/lib/lore
COPY --from=builder /src/target/release/lore /usr/local/bin/lore
ARG VERSION=dev
# The embedding model is baked in under HF_HOME, not under /var/lib/lore: a volume on the
# data directory would hide it.
ENV LORE_VERSION=$VERSION LORE_DATA_DIR=/var/lib/lore HF_HOME=/opt/lore-models
RUN mkdir -p $HF_HOME && chown lore:lore $HF_HOME
USER lore
RUN lore fetch-model
EXPOSE 8080
ENTRYPOINT ["lore"]
CMD ["serve"]
