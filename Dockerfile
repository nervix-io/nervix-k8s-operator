FROM rust:1.97-alpine AS builder

WORKDIR /workspace
RUN apk add --no-cache build-base perl pkgconf
RUN cargo install cargo-auditable --locked
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo auditable build --release

FROM alpine:3.23

RUN apk add --no-cache ca-certificates

COPY --from=builder /workspace/target/release/nervix-k8s-operator /usr/local/bin/nervix-k8s-operator

ENTRYPOINT ["/usr/local/bin/nervix-k8s-operator"]
