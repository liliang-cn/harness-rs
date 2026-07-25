-- Tier-2 增量转换:CDC 落地的 delta(stg_sales_cdc,英文列、全 TEXT)→ 治理仓 sales。
-- 与 Tier-1 同样的校验闸门:类型转换 + 主键去重 + 外键(商品/会员)+ 幂等 upsert。
-- 只处理 sales 事实(维度表变动少,由 Tier-1 全量/单独同步)。
INSERT INTO sales (sale_id, sku, member_id, sold_at, qty, amount, cost_amount)
SELECT DISTINCT ON ((sale_id)::bigint)
  (sale_id)::bigint,
  (sku)::int,
  CASE WHEN member_id ~ '^[0-9]+$' AND (member_id)::int IN (SELECT member_id FROM members)
       THEN (member_id)::int ELSE NULL END,
  (sold_at)::timestamp,
  nullif(qty,'')::int, nullif(amount,'')::numeric, nullif(cost_amount,'')::numeric
FROM stg_sales_cdc
WHERE sale_id ~ '^[0-9]+$'
  AND sku ~ '^[0-9]+$'
  AND (sku)::int IN (SELECT sku FROM products)   -- 外键闸门:不存在的商品被拒
ORDER BY (sale_id)::bigint
ON CONFLICT (sale_id) DO NOTHING;                -- 幂等:重复 sale_id 不重复入库
