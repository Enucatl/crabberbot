# Rust review

Reviewed 2026-09-28 at commit `c5575e2818c8479965920007bc1487a219f6e52f`.

Scope: the current application, downloader worker, premium providers, storage and migrations, tests, and deployment configuration. No PR or proposed diff was supplied, so the README and customer-facing behavior provide the design context: download and cache media, isolate downloader processes, and reliably deliver paid features. This is a review only; no application code was changed.

Findings are ordered by priority. **P1** merits prompt correction because payment or entitlement correctness is affected; **P2** is a concrete correctness, resource-lifetime, or performance issue. Suggested regression checks below are follow-up work, not claims that those scenarios were executed.

## Findings

### 1. P1 — Preserve failed payment fulfillment for retry

**Locations:** `src/storage.rs:399–484`, `src/commands.rs:602–612`, `src/main.rs:367–371`.

`fulfill_payment` returns the same `false` for a duplicate charge and for failures beginning, executing, or committing the transaction. The handler silently returns `Ok(())` in every case. A real payment received during a database outage can therefore be acknowledged without granting the purchase or retaining a recoverable payment record. The checked-out Teloxide webhook implementation acknowledges updates after placing them in an in-memory channel, before this handler runs; merely returning an error from the handler would not cause webhook redelivery.

**Action:** distinguish duplicate fulfillment from database failure with a `Result`. Provide durable payment intake/replay or reconciliation keyed by the existing unique Telegram charge ID, so recovery grants the entitlement exactly once. Apply the same distinction to refund processing, whose boolean result also conflates an already processed refund with a database failure.

**Regression check:** fail fulfillment storage after webhook receipt, recover the database, and verify the paid entitlement eventually appears once; replaying the charge must not grant it again.

### 2. P1 — Serialize automatic refunds with premium work for the same user

**Locations:** `src/commands.rs:408–441`, `src/commands.rs:842–859`, `src/commands.rs:1076–1089`.

Premium callbacks hold a user-specific guard, but `/refundme` does not. Refund eligibility checks only completed usage; transcription and summarization record usage after sending the output. Start a premium action in a group and request a refund in a private chat while the provider is running: the eligibility check can pass, Stars are refunded, and the original action still delivers. The default Teloxide dispatcher serializes by chat, so it does not prevent this cross-chat sequence.

**Action:** use the existing premium user guard in `/refundme` before checking eligibility, retaining it through refund and entitlement revocation. Under the current single-process deployment this closes the demonstrated race without introducing another locking mechanism.

**Regression check:** pause a group transcription before delivery, issue a private-chat refund, and verify refund processing cannot overlap the premium action.

### 3. P1 — Revoke only the entitlement associated with the refunded charge

**Locations:** `src/storage.rs:743–771`, `src/commands.rs:526–536`.

Refunding any subscription charge resets the user's current subscription to free. The owner command allows selecting a historical charge. If a user buys Basic under charge A, later buys Pro under charge B, and A is refunded, the still-paid Pro entitlement disappears. The transaction prevents duplicate revocation but does not establish which purchase owns the current entitlement.

**Action:** associate the current subscription with its originating payment and condition revocation on that association. Preserve a newer paid subscription or independently granted entitlement when refunding an older purchase.

**Regression check:** purchase A and then B; refund A and verify B survives; refund B and verify its entitlement is revoked.

### 4. P2 — A failed typing indicator leaks reserved quota

**Locations:** `src/commands.rs:1190–1209`; callers at `src/commands.rs:1059` and `src/commands.rs:1114`.

`prepare_ai_action` reserves paid seconds and then propagates a `send_chat_action` failure with `?`. The reservation has not yet been returned to either caller, and this exit never calls `release_ai_seconds`. A Telegram timeout or permission error consumes minutes without starting provider work or delivering output.

**Action:** make this optional indicator best effort, send it before reserving quota, or explicitly release the reservation on this error. Keep the correction in the shared preparation function so both actions benefit.

**Regression check:** make the typing request fail for transcription and summarization; neither failure may leave quota debited for undelivered work.

### 5. P2 — URL cleanup destroys media identity

**Locations:** `src/handler.rs:155–173`, `src/handler.rs:495–499`, `src/handler.rs:560–562`.

`cleanup_url` removes every query parameter except YouTube's `v`, and the result is used both as the cache key and as the URL sent to the downloader. Parameters can identify the media or authorize its download. For example, distinct Facebook `/watch/?v=...` links collapse to `/watch/`; signed media URLs lose their signature. This causes failed downloads and can conflate distinct media in the cache.

**Action:** preserve the original URL for fetching. For cache normalization, remove only explicitly known tracking parameters and retain content identifiers and access parameters; do not assume all non-YouTube query strings are disposable.

**Regression check:** two query-selected videos must remain distinct; a signed download URL must reach the downloader with its required parameters intact; known tracking parameters may still normalize where safe.

### 6. P2 — Send generated transcripts and summaries as literal text

**Locations:** `src/handler.rs:731–744`, `src/telegram_api.rs:498–503`; callers at `src/commands.rs:1076` and `src/commands.rs:1131`.

The summarizer returns plain strings, but `send_long_text` forwards them to an API method that always enables HTML parsing. A transcript containing `Vec<String>` can fail delivery, while literal supported tags alter the displayed speech. UTF-8-safe splitting does not make text safe for HTML parsing.

**Action:** use a plain-text send path for generated output, or escape each raw chunk with the existing `teloxide::utils::html::escape` before sending. Escape after splitting, including the short-text branch, so an escaped entity cannot be split between messages.

**Regression check:** short and multi-message output containing `<`, `>`, `&`, and literal tags must arrive unchanged, including characters adjacent to a chunk boundary.

### 7. P2 — Truncate caption text before rendering HTML

**Location:** `src/downloader.rs:155–187`.

`build_caption` first escapes metadata and adds uploader `<i>` tags, then truncates that rendered string with `.chars().take(...)`. The cut can bisect `&amp;` or remove a closing `</i>`. A long uploader or an ampersand at the truncation boundary can therefore make an otherwise valid media upload fail HTML parsing.

**Action:** allocate a text budget to the raw uploader and description, truncate those values, and then escape and wrap them in complete tags. Keep the final caption within Telegram's limit without cutting markup.

**Regression check:** use an uploader longer than the available budget and descriptions placing an escaped character at every possible truncation boundary; all resulting captions must retain valid markup.

### 8. P2 — Honor explicit server retry delays

**Location:** `src/retry.rs:47–49`; consumers include `src/telegram_api.rs:319–322` and both premium provider adapters.

The shared retry helper caps explicit server delays with `max_delay`, as well as capping locally calculated backoff. Provider policies cap at 10 seconds and Telegram at 30 seconds. A Telegram response requesting a 120-second wait can exhaust all four attempts after roughly 90 seconds, entirely before the allowed retry time.

**Action:** cap only locally calculated backoff. Honor a server delay, or return a deferred/error outcome if the caller's deadline cannot accommodate it; do not dispatch an early retry.

**Regression check:** a server delay greater than `max_delay` must not allow another attempt before that delay expires.

### 9. P2 — Move image transformations off Tokio runtime workers

**Locations:** `src/telegram_api.rs:30–46`, `src/telegram_api.rs:89–116`; async callers at `src/telegram_api.rs:402–405`, `src/handler.rs:284`, and `src/handler.rs:362`.

Image decoding, Lanczos resizing, JPEG encoding, and synchronous file writes run directly inside async request handlers. The photo policy permits 48 million pixels, and galleries perform multiple transforms. These operations occupy Tokio runtime threads without yielding, delaying timers, webhook handling, and unrelated network requests under concurrent image traffic. An outer async timeout cannot preempt a synchronous transform.

**Action:** invoke the existing synchronous helpers through `tokio::task::spawn_blocking`, moving an owned `PathBuf` into the closure. Cover thumbnails and both single-photo and gallery paths. Retain the existing image limits.

**Regression check:** run representative large-image processing concurrently with a timer or lightweight request and verify the latter continues making progress.

### 10. P2 — Keep worker jobs and their files owned until handoff

**Locations:** `src/bin/downloader-worker.rs:72–81`, `src/bin/downloader-worker.rs:111`, `src/bin/downloader-worker.rs:145–152`; caller deadline at `src/main.rs:161–173`.

Each socket connection starts a detached task. When the bot's overall timeout drops the socket, the worker continues waiting for permits or downloading because it never observes disconnects during the job. A later successful download can fail its response write, which is ignored, leaving files without an owner. The bot creates its cleanup guard only after receiving the result; worker orphan cleanup runs only at startup.

**Action:** cancel queued/running work when the client disconnects, and keep cleanup responsibility in the worker until the result is handed off. Delete successful results that cannot be delivered, and cover partial artifacts on cancellation.

**Regression check:** disconnect while a job waits for a permit and while it downloads; obsolete work must stop and its UUID-prefixed files must not survive indefinitely.

### 11. P2 — Extend subprocess deadlines to output tasks and descendants

**Location:** `src/downloader.rs:338–389`.

The timeout wraps only `child.wait()`. Once yt-dlp exits, awaiting the stdout/stderr reader tasks has no deadline. A descendant retaining a pipe can keep the request and semaphore permit alive indefinitely. Conversely, the failure path kills only the direct yt-dlp process; an ffmpeg descendant can remain active and keep writing after cleanup. `kill_on_drop(true)` does not own the entire process tree, and dropping reader `JoinHandle`s does not cancel their tasks.

**Action:** keep the deadline active through output collection, stop readers on every early exit, and terminate the command's process group on timeout/cancellation. Account for descendants before treating artifacts as safe to clean up.

**Regression check:** use a fake downloader that forks a long-lived child inheriting output pipes and exits; the operation must still reach its deadline without leaving child processes or reader tasks behind.

### 12. P2 — Give transformed temporary files cancellation-safe ownership

**Locations:** `src/telegram_api.rs:402–423`, `src/handler.rs:284–305`, `src/handler.rs:362–416`; `/tmp` limit at `docker-compose.yml:8`.

Resized photos and prepared thumbnails are deleted only after awaited upload operations return. The overall request timeout can drop those futures before removal. `FileCleanupGuard` owns downloaded originals, not these generated files. Repeated cancelled uploads therefore accumulate files in the application's 256 MiB `/tmp` tmpfs and can make later image preparation fail. Encoding/save failures after creating a file can also bypass cleanup.

**Action:** own generated paths with a cleanup guard from file creation through upload completion, including error and cancellation paths. When moving work to a blocking task, preserve cleanup ownership if its waiting future is dropped.

**Regression check:** drop an upload future after transformation and verify the generated file is removed; also check an encode/write failure after file creation.

### 13. P2 — Abort destructive cache cleanup when the reference query fails

**Location:** `src/main.rs:493–522`.

The audio janitor uses `unwrap_or_default()` on its database query. A database error becomes an empty reference set, so every audio file older than two hours is treated as orphaned and deleted, including files backing live cache entries and premium buttons. A temporary database outage is thus converted into avoidable file loss and repeat downloads.

**Action:** log the reference-query error and skip that cleanup pass. An authoritative empty result and an unavailable database must remain distinct outcomes.

**Regression check:** fail the query while old but referenced audio exists; no file should be removed. A successful query may still identify true orphans.

### 14. P2 — Delete audio only for cache rows actually expired

**Locations:** `src/storage.rs:139–165`, `src/storage.rs:193–196`.

Expiration first selects stale audio paths, separately deletes rows that still match the expiration predicate, and then deletes every file from the original selection. A cache hit between those statements refreshes `last_used_at`, preserving the database row while its audio is removed. A concurrent cache replacement can similarly invalidate the selected path list.

**Action:** use `DELETE ... RETURNING audio_cache_path` to identify files belonging to the rows actually deleted. Respect any independent live callback references before deleting those files.

**Regression check:** refresh an entry while expiration runs and verify a surviving row retains its audio; an entry actually deleted may release its unreferenced file.

### 15. P2 — Include live callback contexts in audio-file retention

**Locations:** `src/main.rs:493–515`, `src/storage.rs:253–258`, `src/storage.rs:865–867`.

The orphan janitor protects only paths referenced by `media_cache`, although callback contexts retain their own audio paths for 24 hours. Two concurrent downloads of the same URL can each create audio and buttons, while the cache upsert retains only the last path. After two hours, the earlier file is eligible for deletion even though its callback remains valid. A failed media-cache write followed by a successful callback-context write creates the same mismatch.

**Action:** include unexpired callback audio paths in the live-reference query and apply that retention rule to both cleanup paths. Treat file ownership consistently across both tables.

**Regression check:** create two live callbacks with different audio paths for one URL, retaining only one path in `media_cache`; both files must survive while their callbacks remain valid.

### 16. P2 — Match refund eligibility to the published feature policy

**Locations:** `src/storage.rs:846–850`, `src/commands.rs:1033–1041`, `src/terms.rs:37`.

The terms define refund-disqualifying AI usage as transcription or summarization, but `has_ai_usage_since` matches every `premium_usage` row. Audio extraction writes an `audio_extract` row, so a Pro customer who only extracts audio is denied the automatic refund promised by the current terms.

**Action:** restrict the eligibility query to the documented feature set. If the intended product rule includes audio extraction, resolve and publish that policy explicitly rather than letting an analytics-table query define it accidentally.

**Regression check:** audio-only usage must follow the published eligibility rule; transcription and summarization must disqualify the refund.

## Assessment against the requested Rust principles

- **Ownership and lifetimes:** the meaningful problems are cancellation ownership of files, subprocesses, reader tasks, and paid reservations. Most inspected clones are shared `Arc`/`Bytes` handles, small enum/path values, bounded metadata, or retry payload ownership. There is no demonstrated reason to introduce `Cow` broadly or remove clones solely because they exist.
- **Unsafe contracts:** no repository-owned `unsafe` blocks or unsafe declarations were found in `src/` or `build.rs`. This does not audit dependency internals.
- **Abstraction clarity:** the service traits are used for async polymorphism and mock-based tests; downloader and audio extractor also have separate local-worker and socket implementations. They earn their place. Storage's boolean outcomes obscure important failure states, as finding 1 demonstrates.
- **Performance by design:** synchronous image processing and jobs that outlive their requests warrant attention before micro-optimizing collections. The per-chat/per-user limiter guards intentionally span work; the important issue is applying them consistently where billing state requires serialization.
- **Idioms and readability:** findings focus on misleading outcome types, resource ownership, and correct API boundaries. Formatting and lint diagnostics are left to rustfmt and Clippy.

## Verification

- Static review traced the reported paths through their callers, migrations, and existing tests. Checked-out Teloxide source was inspected for webhook acknowledgement and dispatcher ordering.
- `cargo fmt --all --check`: passed.
- `cargo clippy --locked --all-targets -- -D warnings`: failed with three existing `clippy::large_enum_variant` diagnostics for `Request`, `Response`, and `ResultData` at `src/worker_protocol.rs:12`, `:29`, and `:36`. These tool-reported diagnostics are not duplicated as review findings; the run stopped at these errors, so it does not establish that later targets are lint-clean.
- `cargo test --locked`: passed, **114 tests, 0 failures**, including the SQLx storage tests, using a disposable PostgreSQL 18 container. The container and its anonymous volume were removed afterward. CI currently uses PostgreSQL 17; this run does not establish version-specific behavior on 17.
- Toolchain: `rustc 1.98.1 (48a229cea 2026-09-01)`. Validation log: `/tmp/crabberbot-astra-review-wake/53a3660e5305.log`.
- No new tests or application changes were made. Suggested regression scenarios above have not been executed.

The checked-in CI test job runs `cargo test`, but does not invoke rustfmt or Clippy. Add those gates if the requested mandatory-tooling policy is to be enforced on every change.
