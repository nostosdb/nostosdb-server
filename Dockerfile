# syntax=docker/dockerfile:1
FROM rust:1.85-bookworm AS build
WORKDIR /source
COPY nostdb-core ./nostdb-core
COPY nostdb-server ./nostdb-server
COPY nostdb-cli ./nostdb-cli
RUN cargo build --locked --release --manifest-path nostdb-server/Cargo.toml --bin nostd \
    && cargo build --locked --release --manifest-path nostdb-cli/Cargo.toml --bin nostdb

FROM debian:bookworm-slim
RUN groupadd --system --gid 1700 nostdb \
    && useradd --system --uid 1700 --gid nostdb --home-dir /var/lib/nostdb --shell /usr/sbin/nologin nostdb \
    && install -d -o nostdb -g nostdb -m 0700 /var/lib/nostdb \
    && install -d -o nostdb -g nostdb -m 0700 /etc/nostdb
COPY --from=build /source/nostdb-server/target/release/nostd /usr/local/bin/nostd
COPY --from=build /source/nostdb-cli/target/release/nostdb /usr/local/bin/nostdb
USER nostdb:nostdb
VOLUME ["/etc/nostdb", "/var/lib/nostdb"]
EXPOSE 7878
ENTRYPOINT ["nostd"]
CMD ["serve", "--config", "/etc/nostdb/server.toml"]
HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
  CMD ["nostdb", "server", "ping", "--server", "nostdb://127.0.0.1:7878", "--credential-file", "/var/lib/nostdb/credentials/client.token"]
