-- 落库转换 + 校验闸门:staging(stg_*,全 TEXT、中文列名)→ 治理仓(强类型、主键、外键)。
-- 这一步是「少量定制」:把该客户的源列名映射到契约列,并做类型转换 + 校验。
-- 校验原则:治理仓的指标默认数据干净,所以脏行在这里被挡下,绝不进治理表。
BEGIN;

-- 部门:去重主键 + 类型转换
INSERT INTO departments (dept_id, name)
SELECT DISTINCT ON (("部门编号")::int) ("部门编号")::int, "部门名称"
FROM stg_departments
WHERE "部门编号" ~ '^[0-9]+$'
ORDER BY ("部门编号")::int
ON CONFLICT (dept_id) DO NOTHING;

-- 商品:外键(部门必须存在)+ 去重 + 类型
INSERT INTO products (sku, name, dept_id, category, price, cost)
SELECT DISTINCT ON (("商品编码")::int)
  ("商品编码")::int, "商品名称", ("部门编号")::int, "品类",
  ("零售价")::numeric, ("进货价")::numeric
FROM stg_products
WHERE "商品编码" ~ '^[0-9]+$'
  AND ("部门编号")::int IN (SELECT dept_id FROM departments)
ORDER BY ("商品编码")::int
ON CONFLICT (sku) DO NOTHING;

-- 会员:去重 + 类型(手机号保留原文,治理层读时按 PII 脱敏)
INSERT INTO members (member_id, phone, tier)
SELECT DISTINCT ON (("会员号")::int) ("会员号")::int, nullif("手机号",''), "等级"
FROM stg_members
WHERE "会员号" ~ '^[0-9]+$'
ORDER BY ("会员号")::int
ON CONFLICT (member_id) DO NOTHING;

-- 库存:外键(商品必须存在)+ 去重 + 类型
INSERT INTO inventory (sku, qty, days_in_stock)
SELECT DISTINCT ON (("商品编码")::int)
  ("商品编码")::int, ("库存量")::int, ("在库天数")::int
FROM stg_inventory
WHERE "商品编码" ~ '^[0-9]+$'
  AND ("商品编码")::int IN (SELECT sku FROM products)
ORDER BY ("商品编码")::int
ON CONFLICT (sku) DO NOTHING;

-- 销售(事实):主键非空去重 + 商品外键必须命中(否则丢弃)+ 会员不存在则置空 + 类型
INSERT INTO sales (sale_id, sku, member_id, sold_at, qty, amount, cost_amount)
SELECT DISTINCT ON (("流水号")::bigint)
  ("流水号")::bigint,
  ("商品编码")::int,
  -- 非会员交易的会员号为空;仅当为合法且存在的会员时才填,否则置空(不因脏值报错)
  CASE WHEN "会员号" ~ '^[0-9]+$' AND ("会员号")::int IN (SELECT member_id FROM members)
       THEN ("会员号")::int ELSE NULL END,
  ("销售时间")::timestamp,
  nullif("数量",'')::int, nullif("金额",'')::numeric, nullif("成本额",'')::numeric
FROM stg_sales
WHERE "流水号" ~ '^[0-9]+$'
  AND "商品编码" ~ '^[0-9]+$'
  AND ("商品编码")::int IN (SELECT sku FROM products)   -- 外键闸门:不存在的商品被拒
ORDER BY ("流水号")::bigint
ON CONFLICT (sale_id) DO NOTHING;

COMMIT;
