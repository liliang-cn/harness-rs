#!/usr/bin/env bash
# Tier-3 webhook 订单接入:电商/收银平台(有赞/美团式)实时 POST 订单 →
# di webhook-ingest 验签 → 校验落治理仓 → aigui 可立即问到。PUSH 链,和 Tier-2 的 pull 互补。
# 前置:先跑过 Tier-1(治理仓 supermart_ingest 有维度+sales)。
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DI="${DI_BIN:-/Users/liliang/Things/AI/base/dataintelligence/di}"
DEST_DB=supermart_ingest
DEST="postgres://reformd:reformd@localhost:47615/$DEST_DB?sslmode=disable"
SECRET="whsec_placeholder_9f3a"
PORT=34210
Q() { docker exec reformd-pg psql -U reformd -d "$DEST_DB" -tAc "$1"; }
gstat() { echo "$(Q 'select count(*) from sales') 行 · 营收 $(Q 'select round(sum(amount)) from sales')"; }
sign() { printf '%s' "$1" | openssl dgst -sha256 -hmac "$SECRET" | sed 's/^.*= //'; }
push() { local b="$1" sig extra="${2:-}"; sig=$(sign "$b"); [ "$extra" = "badsig" ] && sig="deadbeef";
  curl -s -o /dev/null -w "%{http_code}" -m 8 "http://localhost:$PORT/webhook" \
    -H "Content-Type: application/json" -H "X-Signature: $sig" -d "$b"; }

echo "== 启动订单 webhook 接收端(HMAC 验签)=="
DI_WEBHOOK_SECRET="$SECRET" "$DI" webhook-ingest -dest "$DEST" -addr ":$PORT" \
  -required order_id -after "$HERE/transform_orders.sql" > /tmp/di-webhook.log 2>&1 &
WPID=$!; trap 'kill $WPID 2>/dev/null' EXIT
sleep 2; curl -s -m3 "http://localhost:$PORT/healthz" >/dev/null && echo "  接收端就绪 :$PORT"
echo "  治理仓初始: $(gstat)"

echo
echo "== 平台实时推 20 笔订单(签名正确 → 入仓)=="
base=400000; ok=0
for i in $(seq 1 20); do
  oid=$((base+i)); sku=$((1+RANDOM%1500)); mem=$((1+RANDOM%5000)); qty=$((1+RANDOM%5))
  amt=$(awk "BEGIN{printf \"%.2f\", $qty*(5+rand()*80)}"); cost=$(awk "BEGIN{printf \"%.2f\", $qty*(3+rand()*50)}")
  body="{\"order_id\":$oid,\"sku\":$sku,\"member_id\":$mem,\"sold_at\":\"2026-07-19 12:00:00\",\"qty\":$qty,\"amount\":$amt,\"cost_amount\":$cost}"
  code=$(push "$body"); [ "$code" = "200" ] && ok=$((ok+1))
done
echo "  20 笔推送: $ok 笔 200 OK  →  治理仓: $(gstat)"

echo
echo "== 异常处理:验签失败 + 不存在的商品(治理闸门应拒)=="
bad_sig_body="{\"order_id\":400901,\"sku\":42,\"member_id\":1,\"sold_at\":\"2026-07-19 12:00:00\",\"qty\":1,\"amount\":10,\"cost_amount\":6}"
echo "  伪造签名推送 → HTTP $(push "$bad_sig_body" badsig)(应 401,不入仓)"
bad_sku_body="{\"order_id\":400902,\"sku\":999999,\"member_id\":1,\"sold_at\":\"2026-07-19 12:00:00\",\"qty\":1,\"amount\":10,\"cost_amount\":6}"
echo "  签名正确但商品不存在 → HTTP $(push "$bad_sku_body")(200 收下,但外键闸门拒绝入仓)"
echo "  是否入仓了 order 400902? $(Q 'select count(*) from sales where sale_id=400902')(应 0)"

echo
echo "  最终治理仓: $(gstat)"
echo "== 完成:平台推 → 验签 → 校验闸门 → 治理仓,实时、幂等、留痕。aigui(43128)问数即刻反映。 =="