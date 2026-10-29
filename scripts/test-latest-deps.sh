#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

artifact_dir=${1:?Usage: bash scripts/test-latest-deps.sh ARTIFACT_DIR}
mkdir -p "$artifact_dir"
artifact_dir=$(realpath "$artifact_dir")

test_network=$(docker network create "crabberbot-latest-deps-$$")
test_postgres=""
trap 'if [[ -n "$test_postgres" ]]; then docker rm -fv "$test_postgres" >/dev/null 2>&1 || true; fi; docker network rm "$test_network" >/dev/null' EXIT
test_postgres=$(docker run -d --rm --userns=host \
  --network "$test_network" --network-alias postgres \
  --tmpfs /var/lib/postgresql:rw,size=512m \
  -e POSTGRES_PASSWORD=postgres -e POSTGRES_DB=crabberbot postgres:18)
for _attempt in {1..30}; do
  if docker exec "$test_postgres" pg_isready -U postgres -d crabberbot >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
docker exec "$test_postgres" pg_isready -U postgres -d crabberbot

docker run --rm --userns=host --network "$test_network" \
  --mount "type=bind,source=$PWD,target=/source,readonly" \
  --mount "type=bind,source=$artifact_dir,target=/artifacts" \
  -e DATABASE_URL=postgres://postgres:postgres@postgres:5432/crabberbot \
  rust:1-trixie bash -euc '
    mkdir /app
    cp /source/Cargo.toml /source/Cargo.lock /source/build.rs /app/
    cp -a /source/src /source/migrations /app/
    cd /app
    cargo update
    cp Cargo.lock /artifacts/Cargo.lock
    cargo test --locked --all-targets
  '
