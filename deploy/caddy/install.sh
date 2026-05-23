#!/bin/bash
# Install + configure Caddy on the deploy box. Run as root.
#
# Expects in CWD:
#   ./Caddyfile   (in this same directory)
#
# Flow:
#   1. Add the official Caddy apt repo if not present, apt install caddy.
#   2. Drop our Caddyfile to /etc/caddy/Caddyfile.
#   3. `caddy validate` — fail loudly if the file is broken.
#   4. systemctl enable + reload (or restart on cold start).
#   5. Open :80 and :443 on whichever firewall is active.
#
# After the first successful reload Caddy will try to fetch Let's Encrypt
# certs immediately. The HTTP-01 challenge needs :80 reachable from the
# public internet — check your cloud security group, not just the host
# firewall.

set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "✗ must run as root (sudo bash $0)" >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CADDYFILE="$SCRIPT_DIR/Caddyfile"
if [[ ! -f "$CADDYFILE" ]]; then
  echo "✗ Caddyfile missing at $CADDYFILE" >&2
  exit 1
fi

# ── 1. install caddy from the official cloudsmith apt repo ────────
if ! command -v caddy >/dev/null 2>&1; then
  echo "→ installing caddy from official apt repo"
  apt-get update
  apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl gpg
  curl -fsSL 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
    | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -fsSL 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
    > /etc/apt/sources.list.d/caddy-stable.list
  apt-get update
  apt-get install -y caddy
else
  echo "→ caddy already installed ($(caddy version | head -1))"
fi

# ── 2. drop Caddyfile ─────────────────────────────────────────────
install -d -m 0755 /etc/caddy
install -m 0644 "$CADDYFILE" /etc/caddy/Caddyfile
echo "→ /etc/caddy/Caddyfile updated"

# ── 3. validate before reload — broken config = service won't reload ──
if ! caddy validate --config /etc/caddy/Caddyfile; then
  echo "✗ Caddyfile failed validation. Aborting." >&2
  exit 1
fi

# ── 4. reload (zero-downtime) or restart on first install ─────────
systemctl enable caddy
if systemctl is-active --quiet caddy; then
  systemctl reload caddy
  echo "→ caddy reloaded"
else
  systemctl start caddy
  echo "→ caddy started"
fi

# ── 5. firewall: open :80 + :443 on whichever is active ───────────
if systemctl is-active --quiet firewalld 2>/dev/null; then
  firewall-cmd --add-service=http  --permanent
  firewall-cmd --add-service=https --permanent
  firewall-cmd --reload
  echo "→ firewalld: opened 80/tcp + 443/tcp"
elif command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "Status: active"; then
  ufw allow 80/tcp
  ufw allow 443/tcp
  echo "→ ufw: opened 80/tcp + 443/tcp"
else
  echo "→ no host firewall active — cloud security group still needs the rule"
fi

sleep 1
systemctl --no-pager status caddy | head -15
echo
echo "→ next: point your DNS A records at this box, then watch:"
echo "   journalctl -u caddy -f"
echo "   (caddy logs ACME activity in real time — successful cert fetch"
echo "    looks like 'certificate obtained successfully')"
