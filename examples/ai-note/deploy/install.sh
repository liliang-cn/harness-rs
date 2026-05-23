#!/bin/bash
# Run AS ROOT (or via sudo) on the target host.
# Expects in CWD:
#   ./ai-note         (binary, static-pie musl)
#   ./ai-note.service
# And /etc/ai-note.env present (created out-of-band with chmod 600).
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "✗ must run as root (use sudo bash $0)" >&2
  exit 1
fi
if [[ ! -f /etc/ai-note.env ]]; then
  echo "✗ /etc/ai-note.env missing — create it first (chmod 600)" >&2
  exit 1
fi

install -d -m 0755 /opt/ai-note /var/lib/ai-note
install -m 0755 ./ai-note /opt/ai-note/ai-note
install -m 0644 ./ai-note.service /etc/systemd/system/ai-note.service

systemctl daemon-reload
systemctl enable ai-note
systemctl restart ai-note

sleep 2
systemctl --no-pager status ai-note | head -20
echo "---"
echo "→ local probe (via Caddy in front, assuming caddy is up):"
curl -fsS http://127.0.0.1:6755/api/info && echo
