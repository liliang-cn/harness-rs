#!/usr/bin/env bash
# 启动 8 个行业 bi-server（每个承载一个 DI 语义模型 + 数仓），再起一个静态
# 服务器托管 aigui 前端页面。模型统一走 gpt-5.6-luna（cpa.superleo.app）。
#
#   projects/datainside/bi-server/run-all.sh          # 前台，Ctrl-C 全部退出
#
set -euo pipefail

HARNESS="$(cd "$(dirname "$0")/../../.." && pwd)"
DI_BIN="${DI_BIN:-/Users/liliang/Things/AI/base/dataintelligence/di}"
DSN_BASE="${DSN_BASE:-postgres://reformd:reformd@localhost:47615}"
WEB_PORT="${WEB_PORT:-43100}"

export LLM_MODEL="${LLM_MODEL:-gpt-5.6-terra}"   # 快且稳(sonnet 在该网关常撞 cooldown);claude-sonnet-4-6 / 本地 qwen3.5 亦可
export LLM_BASE="${LLM_BASE:-https://cpa.superleo.app/v1}"
export LLM_KEY="${LLM_KEY:?set LLM_KEY to the cpa.superleo.app api key}"
export DI_BIN

# id : model : db : port : role
#   model = @<crate>  →  projects/datainside/<crate>/di/model.yaml
#         = /abs/path →  used as-is (e.g. DI 自带的 fitness 示例模型)
ROWS=(
  "retail-bi-agent:@retail-bi-agent:applehub:43117:finance"
  "supermarket-bi-agent:@supermarket-bi-agent:supermart:43118:finance"
  "teahouse-bi-agent:@teahouse-bi-agent:teahouse:43119:finance"
  "forge-ops-agent:@forge-ops-agent:forge:43120:finance"
  "hotel-revenue-agent:@hotel-revenue-agent:hotel:43121:finance"
  "pharmacy-bi-agent:@pharmacy-bi-agent:pharmacy:43122:finance"
  "autorepair-ops-agent:@autorepair-ops-agent:autorepair:43123:finance"
  "dental-clinic-agent:@dental-clinic-agent:dental:43124:finance"
  "gym-membership-agent:/Users/liliang/Things/AI/base/dataintelligence/examples/fitness/model.yaml:reformd:43125:analyst"
  "warehouse-bi-agent:@warehouse-bi-agent:warehouse:43126:finance"
  "manufacturing-quality-agent:@manufacturing-quality-agent:manufacturing:43127:quality"
  "supermart-ingest:@supermarket-bi-agent:supermart_ingest:43128:finance"
  "ecommerce-bi-agent:@ecommerce-bi-agent:ecommerce:43129:finance"
  "edumind-tutor-agent:@edumind-tutor-agent:edumind:43300:tutor"   # 从 Excel 落库后的治理仓
)

echo "== 编译 bi-server =="
( cd "$HARNESS" && cargo build -q -p bi-server )
BIN="$HARNESS/target/debug/bi-server"

PIDS=()
cleanup() { echo; echo "== 关闭全部 =="; for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT INT TERM

for row in "${ROWS[@]}"; do
  IFS=: read -r id model db port role <<< "$row"
  case "$model" in
    @*) DI_MODEL="$HARNESS/projects/datainside/${model#@}/di/model.yaml" ;;
    *)  DI_MODEL="$model" ;;
  esac
  DI_DSN="$DSN_BASE/$db?sslmode=disable"
  echo "== 启动 $id  →  http://localhost:$port  (db=$db, role=$role) =="
  DI_MODEL="$DI_MODEL" DI_DSN="$DI_DSN" PORT="$port" DI_ROLE="$role" \
    AUDIT="/tmp/bi-server-$id-audit.jsonl" \
    "$BIN" > "/tmp/bi-server-$id.log" 2>&1 &
  PIDS+=($!)
done

sleep 3
# 数据接入状态服务(「数据接入」面板的后端:治理仓汇总 + 各来源 + 水位 + pending 诊断)
ING="$HARNESS/projects/datainside/supermarket-bi-agent/di/ingest"
if docker exec reformd-pg psql -U reformd -lqt 2>/dev/null | grep -qw supermart_ingest; then
  echo "== 数据接入状态服务: http://localhost:34300 =="
  "$DI_BIN" ingest-status \
    -dest "$DSN_BASE/supermart_ingest?sslmode=disable" -addr ":34300" \
    -cdc-source "$DSN_BASE/pos_live?sslmode=disable" -cdc-table sales -cdc-cursor sale_id \
    -action-sync "bash $ING/action_sync.sh" \
    > /tmp/di-ingest-status.log 2>&1 &
  PIDS+=($!)
fi

echo "== aigui 前端: http://localhost:$WEB_PORT/ =="
cd "$HARNESS/projects/datainside/bi-server/web"
python3 -m http.server "$WEB_PORT" > /tmp/bi-web.log 2>&1 &
PIDS+=($!)

echo "== 全部就绪 · 打开 http://localhost:$WEB_PORT/ =="
wait
