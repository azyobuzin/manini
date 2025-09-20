FROM rust:1.90-bookworm AS builder
WORKDIR /src
COPY . .
RUN cargo build --bin manini --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=builder /src/target/release/manini /usr/local/bin/manini
CMD [ "/usr/local/bin/manini" ]

LABEL org.opencontainers.image.source=https://github.com/azyobuzin/manini
