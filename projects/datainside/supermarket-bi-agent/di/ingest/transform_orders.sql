-- Tier-3 webhook 订单转换:平台推送的订单(stg_orders_webhook,JSON 落地、全 TEXT)
-- → 治理仓 sales。同一校验闸门:类型转换 + 主键去重 + 外键(商品/会员)+ 幂等 upsert。
-- 平台字段 order_id 映射到契约列 sale_id。
INSERT INTO sales (sale_id, sku, member_id, sold_at, qty, amount, cost_amount)
SELECT DISTINCT ON ((order_id)::bigint)
  (order_id)::bigint,
  (sku)::int,
  CASE WHEN member_id ~ '^[0-9]+$' AND (member_id)::int IN (SELECT member_id FROM members)
       THEN (member_id)::int ELSE NULL END,
  (sold_at)::timestamp,
  nullif(qty,'')::int, nullif(amount,'')::numeric, nullif(cost_amount,'')::numeric
FROM stg_orders_webhook
WHERE order_id ~ '^[0-9]+$'
  AND sku ~ '^[0-9]+$'
  AND (sku)::int IN (SELECT sku FROM products)   -- 外键闸门:不存在的商品被拒
ON CONFLICT (sale_id) DO NOTHING;                -- 幂等:同一订单重复推送不重复入库
