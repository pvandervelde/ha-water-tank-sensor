# Dockerfile
FROM rust:1.76-slim-bullseye as builder

WORKDIR /usr/src/app
COPY . .
RUN cargo build --release

FROM debian:bullseye-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/app/target/release/env-monitor /usr/local/bin/

ENV RUST_LOG=info
EXPOSE 8080

CMD ["env-monitor"]
