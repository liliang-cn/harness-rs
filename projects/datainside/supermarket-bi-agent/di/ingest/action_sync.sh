#!/usr/bin/env bash
# 面板「立即同步」动作:从活库 pos_live 增量拉取到治理仓(和 Tier-2 同一条 di sync)。
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DI="${DI_BIN:-/Users/liliang/Things/AI/base/dataintelligence/di}"
"$DI" sync \
  -source "postgres://reformd:reformd@localhost:47615/pos_live?sslmode=disable" \
  -table sales -cursor sale_id \
  -dest "postgres://reformd:reformd@localhost:47615/supermart_ingest?sslmode=disable" \
  -required sale_id -after "$HERE/transform_cdc.sql"
