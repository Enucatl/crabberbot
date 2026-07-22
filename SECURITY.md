# Security Notes

## Runtime boundaries

The Rust application does not contain `yt-dlp` or `ffmpeg`. It sends downloader and audio-extraction requests over a Unix socket to `downloader-worker`. That worker is attached only to the internal downloader network and reaches external media URLs through `egress-proxy`; it cannot directly reach the application, PostgreSQL, or the Internet.

The webhook listener uses teloxide's webhook secret validation. PostgreSQL is reachable only on the internal backend network. Premium callback contexts are owned by the requesting Telegram user, and payment fulfilment, quota reservations, and refunds are persisted transactionally.

All Compose services use the shared `docker-compose-security-baseline` hardening profile. Keep image tags and the baseline under regular review; compose configuration is the source of truth for deployed network and container settings.

## Cargo Audit

Local pre-push hooks run:

```bash
cargo audit --deny warnings --ignore RUSTSEC-2023-0071 --ignore RUSTSEC-2026-0173
```

`RUSTSEC-2023-0071` is ignored because it is reported through `rsa` via `sqlx-mysql`. This project only enables PostgreSQL support in `sqlx`, so the vulnerable MySQL code path is not built. Keep this exception narrow: remove the ignore if `sqlx` stops placing the optional MySQL dependency in `Cargo.lock`, or reassess it before enabling MySQL support.

`RUSTSEC-2026-0173` is ignored because it is reported through `proc-macro-error2` via `aquamarine`, a documentation proc-macro pulled in by `teloxide`. It is an unmaintained warning, not a runtime vulnerability. Remove the ignore when `teloxide` stops depending on `aquamarine` or upgrades away from `proc-macro-error2`.
