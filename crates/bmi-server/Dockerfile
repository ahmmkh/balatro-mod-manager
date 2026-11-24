# Build context: the app repo root (the Cargo workspace and Cargo.lock live there).
#   docker buildx build --platform linux/amd64 -f crates/bmi-server/Dockerfile -t bmi-server:dev .
FROM rust:1.99-slim-bookworm AS build
WORKDIR /w
# The workspace also lists the Tauri crates, whose git dependencies cargo would try to fetch
# while resolving. They are never built here, so drop them from the workspace; Cargo.lock keeps
# the resolved versions of everything the server uses (cargo just prunes the unused entries).
COPY Cargo.toml Cargo.lock ./
RUN sed -i -e '/"src-tauri/d' -e '/^bmm-lib = /d' Cargo.toml
COPY crates/bmi-core/Cargo.toml crates/bmi-core/Cargo.toml
COPY crates/bmi-server/Cargo.toml crates/bmi-server/Cargo.toml
# Dependency layer: build with placeholder sources so source edits don't rebuild dependencies.
RUN mkdir -p crates/bmi-core/src crates/bmi-server/src \
 && echo "" > crates/bmi-core/src/lib.rs && echo "" > crates/bmi-server/src/lib.rs \
 && echo "fn main() {}" > crates/bmi-server/src/main.rs \
 && cargo build --release -p bmi-server
COPY crates ./crates
RUN touch crates/bmi-core/src/lib.rs crates/bmi-server/src/lib.rs crates/bmi-server/src/main.rs \
 && cargo build --release -p bmi-server

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends git python3 python3-pil ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --uid 10001 --no-create-home --shell /usr/sbin/nologin bmi \
 && mkdir /data && chown 10001:10001 /data
COPY --from=build /w/target/release/bmi-server /usr/local/bin/bmi-server
USER 10001
ENV DATA_DIR=/data PORT=8080
EXPOSE 8080
VOLUME /data
ENTRYPOINT ["/usr/local/bin/bmi-server"]
