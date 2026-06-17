# Security Notes

## Cargo Audit

Local pre-push hooks run:

```bash
cargo audit --deny warnings --ignore RUSTSEC-2023-0071 --ignore RUSTSEC-2026-0173
```

`RUSTSEC-2023-0071` is ignored because it is reported through `rsa` via `sqlx-mysql`. This project only enables PostgreSQL support in `sqlx`, so the vulnerable MySQL code path is not built. Keep this exception narrow: remove the ignore if `sqlx` stops placing the optional MySQL dependency in `Cargo.lock`, or reassess it before enabling MySQL support.

`RUSTSEC-2026-0173` is ignored because it is reported through `proc-macro-error2` via `aquamarine`, a documentation proc-macro pulled in by `teloxide`. It is an unmaintained warning, not a runtime vulnerability. Remove the ignore when `teloxide` stops depending on `aquamarine` or upgrades away from `proc-macro-error2`.
