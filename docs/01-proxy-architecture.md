# Egress proxy architecture

## Status

Deferred. This is a complete SSRF boundary for `yt-dlp`, but is more
infrastructure than the project needs right now.

## Why a proxy beside the current app is insufficient

`yt-dlp` currently runs in the `crabberbot` container, which also needs direct
access to PostgreSQL and the local Telegram Bot API. If `yt-dlp` bypasses its
proxy setting, it can still reach those private services. A proxy configuration
alone is therefore not an enforcement boundary.

## Design

Move `yt-dlp` and its download-time `ffmpeg` use into a dedicated worker
container. The application and worker communicate through a Unix-domain socket
in a shared Docker volume, not a Docker network.

```text
app container                         downloader worker
-------------                         -----------------
Postgres / local Telegram API         yt-dlp + ffmpeg
        |                                      |
        +-- backend network                     +-- downloader network
               (internal)                              (internal)
                                                       |
                                              egress proxy
                                                       |
                                                Internet network
```

- The worker is attached only to the internal downloader network.
- The proxy is attached to that network and to an Internet-facing network.
- The worker invokes `yt-dlp` with `--proxy http://egress-proxy:3128`.
- The worker has no direct route to the Internet or the app's private services;
  a downloader that ignores proxy settings fails closed.
- Downloads remain in the existing shared downloads volume for the app to
  upload.

## Proxy policy

The forward proxy accepts requests only from the downloader network and:

- allows normal HTTP/HTTPS traffic;
- resolves each requested hostname and rejects loopback, private, link-local,
  Docker-network, multicast, reserved, and IPv6 ULA destinations;
- denies all other traffic and does not expose a host port;
- avoids detailed URL logging.

For HTTPS, the proxy validates the destination before opening a `CONNECT`
tunnel; it does not need TLS interception. Each redirect and media/CDN request
is separately resolved and checked, so private redirects and DNS rebinding are
blocked at connection time.

## Why URL parsing is not sufficient

The application can and should accept only `http` and `https` URLs, but it
cannot safely enforce destination policy by parsing alone. `yt-dlp` performs
its own later DNS lookups and follows redirects to manifests and CDN hosts. A
hostname can resolve publicly during validation and to a private address when
the downloader connects. The proxy evaluates the actual destination for every
connection.

## Cost and tradeoff

This adds no paid service or separate host. It adds a small forward proxy
(typically tens of MiB of memory), a worker service, and a narrow Unix-socket
job protocol. The `yt-dlp`/`ffmpeg` runtime moves from the application to the
worker rather than being duplicated.
