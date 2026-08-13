FROM rust:1.88-alpine AS builder

RUN apk add --no-cache ca-certificates musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --locked --release -p lazybox-relay

FROM scratch
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /src/target/release/lazybox-relay /usr/local/bin/lazybox-relay

ENV LAZYBOX_RELAY_LISTEN_ADDR=0.0.0.0:9443 \
    RUST_LOG=lazybox_relay=info \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
USER 65532:65532
EXPOSE 9443
HEALTHCHECK --interval=15s --timeout=3s --start-period=2s --retries=3 \
    CMD ["/usr/local/bin/lazybox-relay", "--healthcheck"]
ENTRYPOINT ["/usr/local/bin/lazybox-relay"]
