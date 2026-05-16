#!/usr/bin/env bash
# Publish every workspace crate at the current workspace version, in
# topological order. Rate-limit aware (sleeps + retries on 429). Idempotent
# (skips a crate already on crates.io at that version).
#
# Usage: scripts/publish-loop.sh
# Requires: cargo login done first (~/.cargo/credentials.toml)

set -u
cd "$(dirname "$0")/.."

# Read current workspace version from Cargo.toml
VERSION=$(grep -m1 '^version = ' Cargo.toml | head -1 | cut -d'"' -f2)
if [ -z "$VERSION" ]; then echo "could not read workspace version" >&2; exit 1; fi
echo "publishing workspace @ $VERSION"

# Topological order (deps before dependents).
ORDER=(
  harness-rs-core
  harness-rs-macros
  harness-rs-context
  harness-rs-hooks
  harness-rs-models
  harness-rs-sensors-common
  harness-rs-sensors-rust
  harness-rs-skills
  harness-rs-tools-fs
  harness-rs-tools-shell
  harness-rs-compactor
  harness-rs-sandbox
  harness-rs-mcp
  harness-rs
  harness-rs-loop
  harness-rs-blueprint
  harness-rs-templates
  harness-rs-daemon
  harness-rs-cli
)

stamp() { date '+%H:%M:%S'; }

is_published() {
  curl -fsS "https://crates.io/api/v1/crates/$1/$2" >/dev/null 2>&1
}

publish_one() {
  local crate=$1
  if is_published "$crate" "$VERSION"; then
    echo "[$(stamp)] $crate@$VERSION: already published"
    return 0
  fi

  local attempt=0
  local sleep_s=600
  while [ $attempt -lt 30 ]; do
    attempt=$((attempt+1))
    echo "[$(stamp)] $crate: attempt $attempt"
    cargo publish -p "$crate" > /tmp/last-publish.log 2>&1

    sleep 10
    if is_published "$crate" "$VERSION"; then
      echo "[$(stamp)] $crate: ✓ published $VERSION"
      sleep 600  # pace next publish to dodge rate limit
      return 0
    fi

    if grep -q "Too Many Requests" /tmp/last-publish.log; then
      echo "[$(stamp)] $crate: 429 rate-limited, sleeping ${sleep_s}s"
      sleep $sleep_s
      sleep_s=$((sleep_s + 120))
      [ $sleep_s -gt 1800 ] && sleep_s=1800
    else
      echo "[$(stamp)] $crate: ✗ non-rate-limit failure:"
      tail -12 /tmp/last-publish.log | sed 's/^/   /'
      return 1
    fi
  done
  echo "[$(stamp)] $crate: ✗ gave up after $attempt attempts"
  return 1
}

echo "════════════════════════════════════════"
echo "publish loop start at $(date)"
echo "remaining check..."
PENDING=0
for c in "${ORDER[@]}"; do
  if ! is_published "$c" "$VERSION"; then PENDING=$((PENDING+1)); fi
done
echo "$PENDING / ${#ORDER[@]} crates pending"
echo "════════════════════════════════════════"

for c in "${ORDER[@]}"; do
  if ! publish_one "$c"; then
    echo ""
    echo "════════════════════════════════════════"
    echo "STOPPED — $c failed (not rate-limit). Re-run this script to resume."
    echo "════════════════════════════════════════"
    exit 1
  fi
done

echo ""
echo "🎉 all ${#ORDER[@]} crates @ $VERSION published"
echo "finished at $(date)"
