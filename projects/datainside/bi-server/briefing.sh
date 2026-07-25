#!/usr/bin/env bash
# 每日经营简报:对某个 vertical 的治理仓生成一份经营简报(Markdown),存文件,可选推送到
# 钉钉/企业微信 群机器人。数字全部经 DI 治理层查得,不编造。
#
#   briefing.sh <port> <名称>
#   放 cron(每天 8:00):  0 8 * * *  /path/briefing.sh 43128 超市
#   推送:  PUSH_WEBHOOK=<钉钉/企业微信 webhook> briefing.sh 43128 超市
set -euo pipefail
PORT="${1:-43128}"
NAME="${2:-经营}"
OUT="${BRIEF_DIR:-/tmp/briefings}"; mkdir -p "$OUT"
DATE=$(date +%F)

read -r -d '' MSG <<'EOF' || true
生成【经营简报】。用 query_metric 按 grain=month 查最近几个月本模型最重要的 2-3 个经营指标(营收类、毛利率/合格率类、客单价/满座率/动销率类里有哪个用哪个),对比最近两个【完整】月(忽略数据不完整的当月);再按主要业务维度(门店/部门/品类/产线等)查最近完整月,找出表现最好与最差的。然后用中文写一份 30 秒读完的简报:
**总览**:一句话(最近完整月核心指标 + 环比上月 +/-%)。
**关键数字**:一个小表(本月 / 上月 / 环比)。
**值得关注**(2-3 条):最好/最差 + 一句可执行建议。
只用查到的数,环比用两月数字自算,不编造;数据不足就说明。不要输出图表代码块。
EOF

echo "生成 ${NAME} 经营简报(port $PORT)…"
BODY=$(python3 -c 'import json,sys; print(json.dumps({"session_id":"briefing-cron","message":sys.argv[1]}))' "$MSG")
ANS=$(curl -s -m 150 "http://localhost:$PORT/chat" \
  -H "Content-Type: application/json" -H "Authorization: Bearer boss" \
  -d "$BODY" | python3 -c 'import json,sys; print(json.load(sys.stdin).get("answer",""))')

if [ -z "$ANS" ]; then echo "简报生成失败(bi-server 未起或超时?)"; exit 1; fi

FILE="$OUT/${NAME}-${DATE}.md"
printf '# %s 经营简报 · %s\n\n%s\n' "$NAME" "$DATE" "$ANS" > "$FILE"
echo "已保存: $FILE"
echo "----------------------------------------"
echo "$ANS"
echo "----------------------------------------"

# 可选:推送到钉钉/企业微信群机器人(markdown 消息)
if [ -n "${PUSH_WEBHOOK:-}" ]; then
  PAYLOAD=$(python3 -c 'import json,sys; print(json.dumps({"msgtype":"markdown","markdown":{"title":sys.argv[1]+"经营简报","text":sys.argv[2]}}))' "$NAME" "$ANS")
  code=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$PUSH_WEBHOOK" -H "Content-Type: application/json" -d "$PAYLOAD")
  echo "已推送到群机器人 (HTTP $code)"
fi
