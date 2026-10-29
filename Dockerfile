# syntax=docker/dockerfile:1

FROM rust:1.99-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked && install -m 0755 target/release/netpeek /netpeek

FROM alpine:3.20

LABEL org.opencontainers.image.title="netpeek" \
      org.opencontainers.image.description="Kubernetes network diagnostics: netns snapshot, DNS, TCP checks plus the usual net tools" \
      org.opencontainers.image.source="https://github.com/DavoudTeimouri/NetPeek" \
      org.opencontainers.image.licenses="MIT"

# ponytail: no tshark/wireshark (adds ~300MB). Add as a separate `full` tag if packet
# dissection becomes a requirement; tcpdump covers capture today.
RUN apk add --no-cache \
      iproute2 iputils bind-tools netcat-openbsd socat conntrack-tools \
      tcpdump mtr traceroute nmap nmap-scripts \
      curl openssl iperf3 ethtool \
      iptables nftables jq net-tools bridge-utils \
      bash

COPY --from=build /netpeek /usr/local/bin/netpeek

# no ENTRYPOINT: `kubectl run netpeek --rm -it --image=...` drops into a shell with
# all tools; `-- netpeek info` runs a single check.
CMD ["/bin/sh"]
