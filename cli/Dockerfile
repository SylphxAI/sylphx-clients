# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e
# The `sylphx` CLI as a container image: the Release v2 apply Job
# (`papply-<release-id>`) runs `sylphx apply ...` from it inside tenant
# environments. Build context: the repository root (the clients workspace is
# clients/Cargo.toml + clients/Cargo.lock, separate from the platform one).
#
#   docker build -f clients/cli/Dockerfile -t sylphx-cli .
#
# The binary links glibc (the vendored keyring backend builds libdbus from
# source, which is not a musl-clean build), so the runtime is distroless `cc`:
# no shell, CA certificates, a writable /tmp, uid 65532. The Job runs argv
# directly; /dev/termination-log is writable by that uid through the mount.

FROM docker.io/library/rust:1.99.0-bookworm@sha256:59037199c44290f2befcdd58dcc540164763fc296950255aaefeef096a1866b0 AS builder

RUN apt-get update -qq \
 && apt-get install -y --no-install-recommends ca-certificates pkg-config binutils \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src/clients
# The whole clients workspace, so a crate the CLI gains needs no edit here
# (`target/` and `node_modules` stay out through .dockerignore).
COPY clients ./
ENV CARGO_TERM_COLOR=never
RUN cargo build --release --locked -p sylphx-cli \
 && strip target/release/sylphx

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f

COPY --from=builder /src/clients/target/release/sylphx /usr/local/bin/sylphx
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

USER 65532:65532
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt HOME=/tmp
ENTRYPOINT ["/usr/local/bin/sylphx"]
