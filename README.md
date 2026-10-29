# NetPeek

Kubernetes network diagnostics container + CLI.

`netshoot` hands you the tools. NetPeek hands you answers: one command that
prints the netns snapshot, resolves a name, or tells you whether a socket
actually accepts connections — plus the usual net tools when you want to go
deeper.

## Quick start

Interactive shell with every bundled tool (netshoot-style):

```bash
kubectl run netpeek --rm -it --restart=Never \
  --image=ghcr.io/davoudteimouri/netpeek:latest
```

Single check, no shell:

```bash
kubectl run netpeek --rm -i --restart=Never \
  --image=ghcr.io/davoudteimouri/netpeek:latest -- netpeek tcp 10.96.0.1:443
```

Inside an existing pod's network namespace:

```bash
kubectl debug -it my-pod --image=ghcr.io/davoudteimouri/netpeek:latest \
  --target=my-container
```

Local Docker:

```bash
docker run --rm -it ghcr.io/davoudteimouri/netpeek:latest
docker run --rm ghcr.io/davoudteimouri/netpeek:latest netpeek info --json
```

## Commands

| command | what it answers |
| --- | --- |
| `netpeek info` | interfaces, MTU, link state, routes, `/etc/resolv.conf`, listening TCP sockets — the whole netns in one screen |
| `netpeek dns <name>` | does the name resolve, to what addresses, how fast, through which resolver and search domains |
| `netpeek tcp <host:port>` | does the socket accept, latency, and failure class (`refused`, `timeout`, `network-unreachable`, `host-unreachable`) |

Flags: `--json` (machine output), `--timeout <secs>` (TCP connect timeout, default 5), `-v`.

Exit codes: `0` ok, `1` check failed, `2` usage error — safe to gate CI on.

```bash
$ netpeek info
netpeek info
  hostname    netpeek-pod
  kernel      7.0.0-1013-oracle

interfaces
  lo       mtu 65536  unknown  127.0.0.1/8, ::1/128
  eth0     mtu 1500   up       172.17.0.3/16

routes
  default via 172.17.0.1 dev eth0

dns
  nameservers 10.96.0.10
  search      default.svc.cluster.local svc.cluster.local cluster.local
  ndots       5

listeners
  tcp  0.0.0.0:8080
```

## Bundled tools

`ip` `ss` `ifconfig` `route` `arp` `ping` `dig` `nslookup` `nc` `socat`
`tcpdump` `conntrack` `mtr` `traceroute` `nmap` `curl` `openssl` `iperf3`
`ethtool` `iptables` `nft` `jq` `brctl` `bash`

## Capabilities

`tcpdump`, `nmap` SYN scans and raw sockets need `NET_RAW`:

```bash
kubectl run netpeek --rm -it --restart=Never \
  --image=ghcr.io/davoudteimouri/netpeek:latest \
  --overrides='{"spec":{"containers":[{"name":"netpeek","image":"ghcr.io/davoudteimouri/netpeek:latest","stdin":true,"tty":true,"securityContext":{"capabilities":{"add":["NET_RAW","NET_ADMIN"]}}}]}}'
```

## Status

v0.1 — `info`, `dns`, `tcp`, `--json`. No Kubernetes API access yet, so it runs
with zero RBAC and works even when the control plane is the thing that is broken.

Roadmap:

| version | feature | notes |
| --- | --- | --- |
| v0.2 | `netpeek svc <name> -n <ns>` | Service → ClusterIP → EndpointSlices → per-backend TCP probe. Needs `get/list` on `services` and `endpointslices`. |
| v0.2 | `netpeek http <url>` | TLS chain, cert SAN + expiry, timing breakdown |
| v0.2 | `netpeek mtu <target>` | path MTU / PMTU blackhole detection |
| v0.3 | `netpeek netpol` | rule simulation only — cannot prove enforcement without real traffic |
| v0.3 | eBPF trace | tcpconnect/retransmits, needs `NET_ADMIN` + kernel BTF |

## Development

```bash
cargo test          # assert-based checks, no test framework
cargo build --release
docker build -t netpeek .
```

Zero runtime dependencies: stdlib Rust, single static binary. Interface detail
comes from `iproute2` (`ip -o addr show`), which the image bundles.
