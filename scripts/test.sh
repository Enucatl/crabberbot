#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# Real PostgreSQL semantics, with all database files in disposable RAM storage.
test_container=$(docker run -d --rm --network bridge \
  --tmpfs /var/lib/postgresql:rw,size=512m \
  -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=crabberbot \
  -p 127.0.0.1::5432 postgres:18)
trap 'docker rm -fv "$test_container" >/dev/null 2>&1 || true' EXIT
for attempt in {1..30}; do
  if docker exec "$test_container" pg_isready -U postgres -d crabberbot >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
docker exec "$test_container" pg_isready -U postgres -d crabberbot
test_binding=$(docker port "$test_container" 5432/tcp)
export DATABASE_URL="postgres://postgres:postgres@127.0.0.1:${test_binding##*:}/crabberbot"

cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
