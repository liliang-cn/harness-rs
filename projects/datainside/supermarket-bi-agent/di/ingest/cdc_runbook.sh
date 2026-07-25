#!/usr/bin/env bash
# Tier-2 活库 CDC 增量:客户 POS 库(pos_live)持续产生新交易,
# `di sync` 按水位(sale_id)只增量拉取新行 → 校验落治理仓,保持数仓新鲜。
# 前置:先跑过 Tier-1 的 load.sh(治理仓 supermart_ingest 已有维度 + 20万 sales)。
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DI="${DI_BIN:-/Users/liliang/Things/AI/base/dataintelligence/di}"
SRC_DB=pos_live; DEST_DB=supermart_ingest
SRC="postgres://reformd:reformd@localhost:47615/$SRC_DB?sslmode=disable"
DEST="postgres://reformd:reformd@localhost:47615/$DEST_DB?sslmode=disable"
Q() { docker exec reformd-pg psql -U reformd -d "$1" -tAc "$2"; }

echo "== 0. 准备客户 POS 活库 pos_live(= 治理仓当前的 20万 sales,英文强类型) =="
docker exec reformd-pg psql -U reformd -d postgres -c "DROP DATABASE IF EXISTS $SRC_DB" >/dev/null
docker exec reformd-pg createdb -U reformd "$SRC_DB"
# POS 库的 sales(英文强类型,无外键——就是客户业务库的原样);数据从治理仓拷入
docker exec reformd-pg psql -U reformd -q -d "$SRC_DB" -c "CREATE TABLE sales (sale_id bigint PRIMARY KEY, sku int, member_id int, sold_at timestamp, qty int, amount numeric, cost_amount numeric)"
docker exec reformd-pg pg_dump -U reformd --data-only -t sales "$DEST_DB" | docker exec -i reformd-pg psql -U reformd -q -d "$SRC_DB" >/dev/null
echo "   pos_live.sales = $(Q $SRC_DB 'select count(*) from sales') 行,max(sale_id)=$(Q $SRC_DB 'select max(sale_id) from sales')"

gstat() { echo "$(Q $DEST_DB 'select count(*) from sales') 行 · 营收 $(Q $DEST_DB 'select round(sum(amount)) from sales')"; }
echo "   治理仓 sales = $(gstat)"

echo
echo "== 1. 首次 sync(水位=治理仓当前 max,应 0 增量:已最新)=="
"$DI" sync -source "$SRC" -table sales -cursor sale_id -dest "$DEST" -required sale_id -after "$HERE/transform_cdc.sql"

echo
echo "== 2. POS 产生 5000 笔新交易(sale_id 200001..205000) =="
Q $SRC_DB "INSERT INTO sales (sale_id, sku, member_id, sold_at, qty, amount, cost_amount)
  SELECT 200000+g, 1+floor(random()*1500)::int,
    CASE WHEN random()<0.6 THEN 1+floor(random()*5000)::int ELSE NULL END,
    now() - (random()*3||' hours')::interval,
    q, round((q*(5+random()*80))::numeric,2), round((q*(3+random()*50))::numeric,2)
  FROM (SELECT g, 1+floor(random()*5)::int q FROM generate_series(1,5000) g) s" >/dev/null
echo "   pos_live 现有 $(Q $SRC_DB 'select count(*) from sales') 行"
"$DI" sync -source "$SRC" -table sales -cursor sale_id -dest "$DEST" -required sale_id -after "$HERE/transform_cdc.sql"
echo "   → 治理仓 sales = $(gstat)"

echo
echo "== 3. POS 再产生 3000 笔(sale_id 205001..208000) =="
Q $SRC_DB "INSERT INTO sales (sale_id, sku, member_id, sold_at, qty, amount, cost_amount)
  SELECT 205000+g, 1+floor(random()*1500)::int,
    CASE WHEN random()<0.6 THEN 1+floor(random()*5000)::int ELSE NULL END,
    now(), q, round((q*(5+random()*80))::numeric,2), round((q*(3+random()*50))::numeric,2)
  FROM (SELECT g, 1+floor(random()*5)::int q FROM generate_series(1,3000) g) s" >/dev/null
"$DI" sync -source "$SRC" -table sales -cursor sale_id -dest "$DEST" -required sale_id -after "$HERE/transform_cdc.sql"
echo "   → 治理仓 sales = $(gstat)"

echo
echo "== 4. 水位状态(每个源表记录已同步到哪) =="
Q $DEST_DB "select src_table, cursor from _sync_state"
echo "== 完成:活库新交易只增量入仓,水位持久化、幂等、经同一校验闸门。 =="
echo "   连续模式(边写边同步): $DI sync -source $SRC -table sales -cursor sale_id -dest $DEST -required sale_id -after $HERE/transform_cdc.sql -for 30 -every 5"