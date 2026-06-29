#!/usr/bin/env bash
# Start a throwaway PostgreSQL in Docker for the ecommerce-analyst example.
# Idempotent: re-running reuses the existing container.
set -euo pipefail

NAME="harness-ecom-pg"
PORT="${ECOM_PG_PORT:-38520}"
PASS="ecom"
DB="shop"

if docker ps --format '{{.Names}}' | grep -q "^${NAME}$"; then
  echo "✓ container ${NAME} already running on port ${PORT}"
  exit 0
fi

# Remove a stopped container of the same name, if any.
docker rm -f "${NAME}" >/dev/null 2>&1 || true

echo "starting postgres container ${NAME} on host port ${PORT} …"
docker run -d \
  --name "${NAME}" \
  -e POSTGRES_PASSWORD="${PASS}" \
  -e POSTGRES_DB="${DB}" \
  -p "${PORT}:5432" \
  postgres:16-alpine >/dev/null

echo -n "waiting for postgres to accept connections"
for _ in $(seq 1 60); do
  if docker exec "${NAME}" pg_isready -U postgres -d "${DB}" >/dev/null 2>&1; then
    echo " — ready."
    echo
    echo "Connection string:"
    echo "  postgres://postgres:${PASS}@localhost:${PORT}/${DB}"
    echo
    echo "Next:"
    echo "  export DATABASE_URL=postgres://postgres:${PASS}@localhost:${PORT}/${DB}"
    echo "  cargo run -p ecommerce-analyst --bin seed          # generate realistic data"
    echo "  DASHSCOPE_API_KEY=sk-... cargo run -p ecommerce-analyst   # run the analyst"
    exit 0
  fi
  echo -n "."
  sleep 1
done
echo
echo "postgres did not become ready in time" >&2
exit 1
