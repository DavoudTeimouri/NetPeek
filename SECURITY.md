# Security policy

## Reporting a vulnerability

Do not open a public issue for security problems.

Use GitHub's private vulnerability reporting:
<https://github.com/DavoudTeimouri/NetPeek/security/advisories/new>

Include the affected version or image tag, reproduction steps and impact. Expect
an initial response within 7 days.

## What this image is

NetPeek is a privileged debugging tool, not a workload. Treat it accordingly:

- It bundles tools that need `NET_RAW`/`NET_ADMIN` (`tcpdump`, `nmap`, `iptables`,
  `nft`). Run it on demand — `kubectl run` or `kubectl debug` — not as a
  long-running sidecar or DaemonSet.
- `netpeek svc` reads the pod's service-account token from
  `/var/run/secrets/kubernetes.io/serviceaccount/token` and sends it only to
  `https://kubernetes.default.svc`. It needs just `get services` and
  `list endpointslices`; anything wider is unnecessary.
- The default image is rebuilt from `alpine:3.20` on every push to `main`. That
  picks up base-image fixes but is not reproducible. For pinned deployments use a
  `sha-*` tag, a released version tag, or a digest.
- `netpeek http` shells out to `curl` and `openssl`; both come from Alpine
  packages in this image. If you mount your own binaries over them, you own the
  result.
