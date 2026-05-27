#!/bin/bash
# Run AS ROOT (or via sudo) on the target host.
# Expects in CWD:
#   ./dashboard          (binary, static-pie musl)
#   ./dashboard.service
# And /etc/ai-ledger.env present (created out-of-band with chmod 600).
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "✗ must run as root (use sudo bash $0)" >&2
  exit 1
fi
if [[ ! -f /etc/ai-ledger.env ]]; then
  echo "✗ /etc/ai-ledger.env missing — create it first (chmod 600)" >&2
  exit 1
fi

install -d -m 0755 /opt/dashboard /var/lib/ai-ledger
install -m 0755 ./dashboard /opt/dashboard/dashboard
install -m 0644 ./dashboard.service /etc/systemd/system/dashboard.service

systemctl daemon-reload
systemctl enable dashboard
systemctl restart dashboard

# Firewall: open 6743 on whichever firewall is active.
if systemctl is-active --quiet firewalld 2>/dev/null; then
  firewall-cmd --add-port=6743/tcp --permanent
  firewall-cmd --reload
  echo "→ firewalld: opened 6743/tcp"
elif command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "Status: active"; then
  ufw allow 6743/tcp
  echo "→ ufw: opened 6743/tcp"
else
  echo "→ no host firewall active — cloud security-group still needs the rule"
fi

sleep 2
systemctl --no-pager status dashboard | head -20
echo "---"
echo "→ local probe:"
curl -fsS http://127.0.0.1:6743/api/info && echo
