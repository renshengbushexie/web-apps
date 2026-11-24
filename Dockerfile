FROM rust:1.100-alpine AS build
RUN apk add --no-cache musl-dev build-base cmake ca-certificates perl
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bins

FROM alpine:3.23
RUN apk add --no-cache ca-certificates && adduser -D -H -u 10001 kb && mkdir /data && chown kb /data
COPY --from=build /build/target/release/kb-api /build/target/release/kb-sync /usr/local/bin/
USER kb
ENV KB_DB=/data/knowledge.sqlite3 KB_SYNC_DB=/data/sync.sqlite3 KB_BIND=0.0.0.0:8080
WORKDIR /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/kb-api"]
