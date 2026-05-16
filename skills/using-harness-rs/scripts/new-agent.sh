#!/usr/bin/env bash
# Bootstrap a new harness-rs agent.
#
# Usage:
#   new-agent.sh <name> [parent-dir]
#
# Wraps `harness new --local`, then prints the next step. Use this script when
# the user is on a machine that has the harness CLI installed but doesn't
# remember the exact flag.

set -euo pipefail

NAME=${1:-}
PARENT=${2:-$PWD}

if [ -z "$NAME" ]; then
    echo "usage: $0 <name> [parent-dir]" >&2
    exit 1
fi

if ! command -v harness >/dev/null 2>&1; then
    echo "harness CLI not found. Install with:" >&2
    echo "  cargo install harness-rs-cli" >&2
    exit 1
fi

echo "→ creating $PARENT/$NAME via harness new --local"
harness new "$NAME" --path "$PARENT" --local

echo
echo "✓ created. Next:"
echo "    cd $PARENT/$NAME"
echo "    export DEEPSEEK_API_KEY=…    # or ANTHROPIC_API_KEY"
echo "    cargo run"
echo
echo "Then edit src/main.rs to change the task, add tools, swap the model."
