# Senior Rust Architecture and Security Review

## Executive summary

The repository is understandable, uses safe Rust throughout, parameterizes SQL correctly, runs as a non-root container user, authenticates Telegram webhooks through teloxide’s generated secret, and has a solid unit-test baseline.

The review found:

- 9 high-severity issues
- 9 medium-severity issues
- 5 low-severity issues
- No direct memory-safety or `unsafe`-Rust findings
- No known exploitable Rust dependency advisory
- No SQL or shell injection

The most urgent risks are unrestricted SSRF through `yt-dlp`, non-idempotent payment fulfillment, missing callback ownership checks, free repeated cached AI actions, global resource exhaustion, and mutable privileged CI dependencies.

No files were modified during the review.

## Architecture and trust boundaries

```text
Telegram
   │ authenticated webhook
   ▼
Axum/teloxide dispatcher
   ├── Commands and payments ──► PostgreSQL
   ├── Callback actions ───────► cached audio/transcripts
   │                              ├── Deepgram
   │                              └── Gemini
   └── User URL ───────────────► yt-dlp
                                  ├── arbitrary outbound HTTP
                                  ├── downloaded files
                                  └── ffprobe/ffmpeg
                                         │
                                         ▼
                                  Telegram Bot API
```

## High-severity findings

### H1. Block private-network destinations before invoking `yt-dlp`

- Confidence: High
- Category: Security / Availability
- Location: `src/main.rs:422`, `src/downloader.rs:409`
- Evidence: Any string accepted by `Url::parse` is passed verbatim to `yt-dlp`.
- Preconditions: Any Telegram user; the container can reach internal services.
- Exploit path: submit an HTTP URL targeting loopback, private, link-local, metadata, or internal service; `yt-dlp` fetches it server-side; the attacker probes services or retrieves media-like responses.
- Impact: SSRF, internal discovery, possible data disclosure, internal-service DoS.
- Recommendation: Accept only HTTP/HTTPS and enforce outbound network policy preventing private/link-local/loopback access, including redirects and DNS rebinding. A host allowlist is stronger if product scope permits.
- Verification: Redirect a public test endpoint to `127.0.0.1`; the internal endpoint must receive no request.

### H2. Make payment fulfillment transactional and idempotent

- Confidence: High
- Category: Security / Correctness
- Location: `src/commands.rs:606`, `src/storage.rs:377`
- Evidence: Duplicate charge insertion is ignored with `ON CONFLICT DO NOTHING`, but entitlement is granted unconditionally afterward.
- Exploit path: Telegram retries or replays an already processed successful-payment update; payment insertion is ignored; the subscription is reset or another 3,600 top-up seconds are granted.
- Impact: Free repeated credit or subscription renewal.
- Recommendation: In one database transaction, insert the charge and grant entitlement only when `INSERT ... RETURNING` confirms a new charge.
- Verification: Process the same charge twice; assert one payment and one entitlement grant.

### H3. Bind premium callbacks to their originating chat and user

- Confidence: High
- Category: Security / Privacy
- Location: `src/commands.rs:754`, `src/storage.rs:464`, `src/handler.rs:756`
- Evidence: Callback context stores `chat_id`, but it is never compared with the callback message. No originating user is stored.
- Exploit path: another group member or recipient of an accessible button clicks it; the context is loaded solely by ID; cached audio/transcript is processed and returned in the clicking message’s chat.
- Impact: Cross-user activation and possible disclosure of cached content.
- Recommendation: Require callback chat equality and store/check an originating `user_id` if buttons are requester-only. Use opaque random tokens if callbacks can escape their original message context.
- Verification: Wrong-chat and wrong-user callbacks must invoke no file, provider, or delivery operation.

### H4. Charge cached AI actions consistently

- Confidence: High
- Category: Security / Cost / Correctness
- Location: `src/commands.rs:1164`
- Evidence: Quota is consumed only when Deepgram runs. Cached transcripts still invoke Gemini but produce `DeepgramUsage::None`, suppressing the duration charge.
- Exploit path: transcribe once; repeatedly request correction or summarization using the cached transcript; receive paid Gemini work without consuming video-minute quota.
- Impact: Unlimited provider expense for previously transcribed media.
- Recommendation: Charge once per successfully delivered requested action, independent of whether its transcript was cached. Record Deepgram costs separately.
- Verification: Cached transcription and summarization must each consume the intended duration exactly once.

### H5. Add a global download/process concurrency limit

- Confidence: High
- Category: Availability
- Location: `src/main.rs:136`, `src/downloader.rs:415`
- Evidence: Concurrency is limited only per chat; each distinct chat can start a six-minute pipeline.
- Exploit path: many accounts or groups submit slow URLs concurrently; each spawns `yt-dlp` and may consume disk, memory, network, upload, and ffmpeg resources.
- Impact: Process, file-descriptor, disk, bandwidth, or memory exhaustion.
- Recommendation: Add one bounded global semaphore around the complete download pipeline while keeping the per-chat guard.
- Verification: N simultaneous chats must never create more than the configured number of `yt-dlp` jobs.

### H6. Make metadata validation fail closed

- Confidence: High
- Category: Availability / Correctness
- Location: `src/validator.rs:22`
- Evidence: Missing metadata is accepted; `NaN` duration bypasses `duration > limit`; playlist entries are not individually checked; playlist type and limit depend only on the first entry; the playlist branch skips root duration and size.
- Exploit path: select media with missing, non-finite, heterogeneous, or inaccurate metadata; validation passes; much larger downloads proceed.
- Impact: Disk, memory, CPU, and network exhaustion.
- Recommendation: Reject invalid/non-finite/negative values, validate every entry, conservatively classify unknown playlists, and enforce a real byte limit during download rather than relying on approximate metadata.
- Verification: Cover `NaN`, negative/missing values, mixed playlists, oversized entries, and enforced download-byte termination.

### H7. Pin privileged CI dependencies to commit SHAs

- Confidence: High
- Category: Security / Supply chain
- Location: `.github/workflows/deploy.yml:19`, `.github/workflows/latest-deps.yml:16`
- Evidence: Actions and reusable workflows use mutable `@main`, `@stable`, and tags. The reusable deployment receives inherited secrets and package/OIDC write permissions.
- Exploit path: an upstream branch or tag is compromised or moved; privileged foreign code runs in CI.
- Impact: Secret theft, malicious image publication, OIDC abuse, deployment compromise.
- Recommendation: Pin every `uses:` reference to a reviewed full commit SHA.
- Verification: Add a CI policy rejecting non-SHA action references.

### H8. Keep the Gemini key out of request URLs

- Confidence: High
- Category: Security / Confidentiality
- Location: `src/premium/summarizer.rs:71`, `src/retry.rs:50`
- Evidence: The key is included in the URL query. Retried reqwest errors are logged using `Display`, which may include the URL.
- Exploit path: transient network/request failure; formatted error contains URL; key enters application logs.
- Impact: Credential disclosure and billable API abuse.
- Recommendation: Send the key using Google’s `x-goog-api-key` header and keep request URLs secret-free.
- Verification: Trigger a request failure using a sentinel key and assert it never appears in formatted errors or logs.

### H9. Make refunds payment-specific and idempotent

- Confidence: High
- Category: Security / Correctness
- Location: `src/commands.rs:380`, `src/storage.rs:551`
- Evidence: Payments have no refund state. Refunds revoke the current pooled entitlement based on supplied/update product data, not a uniquely transitioned payment record.
- Exploit path: duplicate refund update or refund of an older purchase after a newer one; current subscription or pooled top-up is revoked again.
- Impact: Legitimately purchased access is removed; accounting cannot distinguish refunded charges.
- Recommendation: Add `refunded_at`; atomically transition the matching recorded charge only once and revoke from its stored product.
- Verification: A repeated refund must be a no-op; refunding an older charge must not incorrectly erase a newer entitlement.

## Medium-severity findings

### M1. Bound `yt-dlp` stdout and stderr

`Command::output()` buffers all metadata and download output before parsing. A malicious extractor response can exhaust memory before validation. Stream with a byte cap and terminate the child when exceeded.

Locations: `src/downloader.rs:415`, `src/downloader.rs:474`.

### M2. Stop collapsing meaningful URL queries into one cache key

All non-YouTube query parameters are removed; only the first YouTube `v` is retained. Distinct signed or resource-selecting URLs can collide and return another resource’s cached media. `ends_with("youtube.com")` also matches `notyoutube.com`.

Location: `src/handler.rs:142`.

Recommendation: Preserve queries by default and remove only known tracking parameters for explicitly matched hosts.

### M3. Constrain subprocess-reported paths

Absolute paths returned by `yt-dlp` are accepted, later uploaded, and deleted. A compromised extractor or binary could disclose or delete any app-readable file.

Locations: `src/downloader.rs:253`, `src/handler.rs:55`.

Recommendation: Canonicalize and require regular files beneath the download directory with the current request UUID prefix; reject symlink escapes.

### M4. Honor expiry for unlimited audio tiers

`Pro` and `Ultra` audio extraction checks only the tier, not `expires_at`. Expired rows retain unlimited audio indefinitely.

Location: `src/subscription.rs:136`.

### M5. Validate positive billing durations and add database constraints

Zero or negative duration passes quota checks. Negative consumption can reduce usage or increase available credit. Add application checks and database `CHECK` constraints for durations, balances, amounts, tiers, and products.

Locations: `src/subscription.rs:123`, `src/storage.rs:401`.

### M6. Reserve quota atomically in PostgreSQL

The application reads quota, performs provider work, then consumes quota. The in-memory lock does not protect multiple replicas and storage failures are logged without preventing delivery.

Locations: `src/commands.rs:1164`, `src/storage.rs:401`.

Recommendation: Use a conditional database update or reservation before paid work.

### M7. Bound `ffprobe` and `ffmpeg` execution time

Both subprocesses can run indefinitely while holding one of three extraction permits.

Location: `src/premium/audio_extractor.rs:56`.

Recommendation: Add timeouts and child termination with `kill_on_drop`.

### M8. Pin build and runtime inputs

Base images, production images, `yt-dlp` fallback, apt packages, and Python packages use mutable tags/ranges.

Locations: `Dockerfile:2`, `docker-compose.yml:6`.

Recommendation: Pin deployed images by digest, require a `yt-dlp` commit, and lock Python packages with hashes.

### M9. Protect Terraform secret export

The helper writes Vault values through `tee`, exposing them on stdout and creating the file under the caller’s umask.

Location: `cloudflare-terraform/create_tfvars.sh:1`.

Recommendation: Set `umask 077` and redirect directly to the file, or avoid persistence using `TF_VAR_*`.

## Low-severity findings

- User-controlled support text and usernames are inserted into Telegram HTML without escaping, allowing formatting spoofing or message rejection. `src/commands.rs:286`
- Telegram’s per-chat rate-limit map never evicts entries and grows with every chat contacted. `src/telegram_api.rs:337`
- Successful partial playlist mappings can leave unmatched UUID files until process restart. `src/downloader.rs:493`
- Terraform references a nonexistent `google_service_account.bot_sa`, preventing validation/planning. `cloudflare-terraform/outputs.tf:13`
- Provider audio buffers are cloned for retries and provider response bodies have no explicit byte cap. `src/premium/transcriber.rs:82`

## Unsafe Rust and soundness

- No `unsafe` blocks or unsafe trait implementations were found.
- No unsound `Send`, `Sync`, lifetime, pinning, or aliasing behavior was identified.
- Shell injection was rejected: arguments bypass a shell.
- The primary filesystem concern is trusting paths reported by the subprocess, not Rust memory safety.

## Dependency and supply-chain assessment

- `cargo audit`: no known exploitable vulnerability.
- One documented allowed warning: `RUSTSEC-2026-0173`, unmaintained `proc-macro-error2` through teloxide documentation dependencies.
- `cargo deny` is not installed.
- `Cargo.lock` is committed.
- Duplicate crates are largely ecosystem-level duplication rather than actionable application bloat.
- The main supply-chain exposure is mutable CI, image, `yt-dlp`, and Python dependency references.

## Concurrency and async assessment

Positive:

- Per-chat download and per-user premium guards use RAII correctly.
- Semaphore-based audio extraction bounds active ffmpeg work.
- Tokio mutex use in the rate limiter is deliberate queue serialization, not an OS-thread blocking lock.

Risks:

- No global download bound.
- ffmpeg subprocesses can retain permits indefinitely.
- Quota locking is process-local.
- Telegram per-chat limiter state never expires.
- Provider work and quota deduction are not one atomic operation.

## Performance and resource exhaustion

Highest-value improvements:

1. Global pipeline semaphore.
2. Real download byte/disk quota.
3. Capped subprocess output.
4. ffmpeg/ffprobe timeouts.
5. Cheap shared audio bodies rather than cloning `Vec<u8>` per retry.
6. Provider response-size caps.

Potential future database indexes:

- `payments(user_id, created_at DESC)`
- `premium_usage(user_id, created_at)`

Add these only when table growth makes the scans measurable.

## Idiomatic Rust and simplification opportunities

- Replace the specialized album splitting calculation with `items.chunks(10)`.
- Reject invalid paths instead of rewriting `..` components.
- `build.rs` is confused and likely removable: Cargo already provides `CARGO_PKG_VERSION`, while the script exports a different name.
- Narrowing Tokio’s `full` feature set is only worthwhile after measuring build or binary-size impact.
- Strict Clippy reports 12 issues, mostly collapsible branches, redundant bindings, documentation formatting, and three large-argument functions. The argument-count warnings alone do not justify introducing new abstractions.

A policy inconsistency also needs resolution: `src/terms.rs:23` says audio extraction does not consume AI minutes, while the Basic plan implementation and other text say that it does.

## Verification results

| Command | Result |
|---|---|
| `cargo metadata --format-version 1 --no-deps` | Passed |
| `cargo check --all-targets --all-features` | Passed |
| `cargo test --all-targets --all-features` | 89 passed |
| `cargo fmt --all -- --check` | Passed |
| `cargo clippy --all-targets --all-features -- -D warnings` | Failed: 12 lints |
| `cargo audit` | No vulnerabilities; one allowed unmaintained warning |
| `cargo deny check` | Unavailable |
| `terraform validate` | Unavailable: Terraform not installed |

The checks left the working tree clean.

## File coverage

| Reviewer | Complete assignment |
|---|---|
| Primary + media reviewer | `src/main.rs`, `lib.rs`, `downloader.rs`, `handler.rs`, `validator.rs`, `test_utils.rs`, embedded tests |
| Billing/storage reviewer | `commands.rs`, `storage.rs`, `subscription.rs`, `terms.rs`, all seven migrations, embedded tests |
| Platform/supply reviewer | `telegram_api.rs`, `concurrency.rs`, `config.rs`, `retry.rs`, all `src/premium/*`, Cargo files, build script, Docker/Compose, workflows, pre-commit, gitignore, Terraform, README, security and architecture docs, dashboards, license, image metadata |

All tracked source, build, deployment, migration, workflow, configuration, documentation, monitoring, and architecture assets were assigned and reviewed.

## Rejected or downgraded hypotheses

- No shell injection: subprocess arguments use `Command::arg`.
- No SQL injection: application values use bind parameters.
- Caption metadata is HTML-escaped correctly.
- Relative `..` path traversal is rebased, though absolute and symlink path trust remains.
- Webhook authentication is present: teloxide generates a random secret and validates the Telegram header.
- Sequential callback IDs alone are not directly forgeable through an ordinary Telegram client; the confirmed issue is accessible buttons lacking chat/user authorization.
- Holding the Telegram limiter mutex during sleep deliberately preserves rate spacing.
- External Compose hardening could not be assessed because the extended baseline file is outside this repository.

## Top five remediation actions

1. Restrict `yt-dlp` network egress and add global concurrency/output/download-size limits.
2. Make payment grant and refund transitions transactional and idempotent.
3. Bind callbacks to their originating chat/user and use opaque context tokens.
4. Redesign quota charging as an atomic reservation and charge cached actions consistently.
5. Remove secrets from URLs and pin all privileged CI/build/deployment inputs.
