#!/usr/bin/env bash
# Tier-1 落库流程(标品):源文件 → di ingest 落 staging(原始、全 TEXT)→ 校验转换 → 治理仓。
# 数据来源 = 客户导出的中文表头 CSV;落库 = staging + 校验闸门 + 强类型治理表。
#   projects/datainside/supermarket-bi-agent/di/ingest/load.sh
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DI="${DI_BIN:-/Users/liliang/Things/AI/base/dataintelligence/di}"
PGC="docker exec -i reformd-pg psql -U reformd"
DB="${INGEST_DB:-supermart_ingest}"
DSN="postgres://reformd:reformd@localhost:47615/$DB?sslmode=disable"
SRC="$HERE/source"

echo "== 0. 准备治理仓 $DB(空、强类型) =="
docker exec reformd-pg psql -U reformd -d postgres -c "DROP DATABASE IF EXISTS $DB" >/dev/null
docker exec reformd-pg createdb -U reformd "$DB"
$PGC -d "$DB" -q < "$HERE/warehouse.sql"

[ -f "$SRC/销售流水.csv" ] || { echo "源文件缺失,先跑 export_source.sh"; exit 1; }

echo "== 1. 落 staging(di ingest:中文表头 → 原始 TEXT 表,含 required 校验) =="
ing() { echo "  · $1 → stg_$2"; "$DI" ingest -dsn "$DSN" -csv "$SRC/$1" -table "stg_$2" ${3:+-required "$3"} 2>&1 | sed 's/^/    /'; }
ing 部门表.csv     departments
ing 商品档案.csv   products    商品编码
ing 会员.csv       members     会员号
ing 库存.csv       inventory   商品编码
ing 销售流水.csv   sales       流水号

echo "== 2. staging 行数 =="
for t in departments products members inventory sales; do
  printf "  stg_%-12s %s\n" "$t" "$($PGC -d "$DB" -tAc "select count(*) from stg_$t")"
done

echo "== 3. 校验转换:staging → 治理仓(类型转换 + 主键去重 + 外键闸门) =="
$PGC -d "$DB" -q < "$HERE/transform.sql"

echo "== 4. 治理仓行数(对比 staging 看被拒/去重的行) =="
for t in departments products members inventory sales; do
  s=$($PGC -d "$DB" -tAc "select count(*) from stg_$t")
  g=$($PGC -d "$DB" -tAc "select count(*) from $t")
  printf "  %-12s staging=%-7s 治理仓=%-7s  拒绝/去重=%s\n" "$t" "$s" "$g" "$((s-g))"
done

# 记一条落库留痕(供「数据接入」面板展示)
$PGC -d "$DB" -q -c "CREATE TABLE IF NOT EXISTS _ingest_log (id bigserial PRIMARY KEY, source_type text, note text, rows bigint, watermark bigint, ts timestamptz DEFAULT now())" >/dev/null
$PGC -d "$DB" -q -c "INSERT INTO _ingest_log (source_type, note, rows) VALUES ('file', 'Excel/CSV 全量落库', (SELECT count(*) FROM sales))" >/dev/null

echo "== 5. 治理查询验证(经语义层,按部门营收) =="
"$DI" query -model "$HERE/../model.yaml" -dsn "$DSN" -role finance -metrics revenue,margin_rate -by dept_name 2>&1 | head -12

echo "== 完成:数据来源 → 落 staging → 校验 → 治理仓 → 可治理问数,全链闭环。 =="
echo "   起服务: DI_MODEL=$HERE/../model.yaml DI_DSN=$DSN DI_ROLE=finance PORT=43128 cargo run -p bi-server"
