#!/usr/bin/env bash
# 客户从 POS / 进销存导出的原始文件:中文表头、与治理仓列名不一致,
# 含几条脏数据(空主键 / 不存在的商品 / 重复流水号),用于验证落库的校验闸门。
# 从已种子化的 supermart 库导出(真实分布、真实量),销售明细取样 2 万条。
set -euo pipefail
PG="docker exec -i reformd-pg psql -U reformd -d supermart -tAc"
OUT="$(cd "$(dirname "$0")" && pwd)/source"
mkdir -p "$OUT"
copy() { docker exec reformd-pg psql -U reformd -d supermart -c "\copy ($1) TO STDOUT WITH CSV HEADER" > "$OUT/$2"; }

copy "SELECT dept_id AS \"部门编号\", name AS \"部门名称\" FROM departments ORDER BY dept_id" 部门表.csv
copy "SELECT sku AS \"商品编码\", name AS \"商品名称\", dept_id AS \"部门编号\", category AS \"品类\", price AS \"零售价\", cost AS \"进货价\" FROM products ORDER BY sku" 商品档案.csv
copy "SELECT member_id AS \"会员号\", phone AS \"手机号\", tier AS \"等级\" FROM members ORDER BY member_id" 会员.csv
copy "SELECT sku AS \"商品编码\", qty AS \"库存量\", days_in_stock AS \"在库天数\" FROM inventory ORDER BY sku" 库存.csv
copy "SELECT sale_id AS \"流水号\", sku AS \"商品编码\", member_id AS \"会员号\", sold_at AS \"销售时间\", qty AS \"数量\", amount AS \"金额\", cost_amount AS \"成本额\" FROM sales ORDER BY sale_id LIMIT 200000" 销售流水.csv

# —— 掺 3 条脏数据到销售流水,验证校验闸门会挡住它们 ——
{
  echo '999999001,999999,123,2025-06-01 10:00:00,2,88.00,50.00'   # 商品编码不存在 → 外键校验拒绝
  echo ',12,34,2025-06-01 11:00:00,1,9.90,6.00'                    # 流水号为空 → required 校验丢弃
  echo '1,12,34,2025-06-01 12:00:00,9,999.00,600.00'              # 流水号=1 与已有重复 → 主键去重
} >> "$OUT/销售流水.csv"

echo "已导出源文件到 $OUT :"
wc -l "$OUT"/*.csv | sed 's#.*/##'