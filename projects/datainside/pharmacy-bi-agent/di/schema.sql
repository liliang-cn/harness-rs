-- 连锁药店: many stores × many SKUs, ~150k sales. sales grain vs inventory grain
-- -> sell_through is chasm-safe. revenue/gross_margin/margin_rate are finance-gated.
-- Pharmacy specifics: 处方/OTC split (rx_ratio) and 近效期 stock (near_expiry_stock).
DROP TABLE IF EXISTS sales, inventory, drugs, members, stores CASCADE;
CREATE TABLE stores (store_id int PRIMARY KEY, name text, city text);
CREATE TABLE drugs (sku int PRIMARY KEY, name text, category text, is_rx boolean, base_price numeric);
CREATE TABLE members (member_id int PRIMARY KEY, name text, phone text, tier text);
CREATE TABLE sales (
  sale_id bigint PRIMARY KEY, store_id int REFERENCES stores, sku int REFERENCES drugs,
  member_id int REFERENCES members, sold_at timestamp,
  qty int, amount numeric, cost_amount numeric, is_rx boolean);
CREATE TABLE inventory (
  inv_id serial PRIMARY KEY, store_id int REFERENCES stores, sku int REFERENCES drugs,
  qty int, expiry date);

SELECT setseed(0.41);
INSERT INTO stores
SELECT g, '门店-'||g, (ARRAY['北京','上海','广州','深圳','成都','杭州','武汉'])[1+(g%7)]
FROM generate_series(1,15) g;

-- ~40% 处方药 (['处方药','处方药'] out of 5); category-dependent price so margin varies.
INSERT INTO drugs
SELECT t.g, '药品-'||t.g, t.category, (t.category = '处方药'),
  round((CASE t.category
    WHEN '处方药' THEN 20 + random()*380
    WHEN 'OTC'    THEN 8  + random()*60
    WHEN '中成药' THEN 15 + random()*140
    ELSE               30 + random()*220   -- 保健品
  END)::numeric, 2)
FROM (
  SELECT g, (ARRAY['处方药','处方药','OTC','中成药','保健品'])[1+floor(random()*5)::int] AS category
  FROM generate_series(1,1200) g
) t;

-- ~30% of members carry a CN mobile (PII), the rest NULL.
INSERT INTO members
SELECT g, '会员-'||g,
  CASE WHEN random()<0.3
    THEN '1'||(3+floor(random()*7))::int||lpad(floor(random()*1000000000)::text,9,'0')
    ELSE NULL END,
  (ARRAY['普通','普通','银卡','金卡'])[1+(g%4)]
FROM generate_series(1,40000) g;

-- inventory grain: on-hand per store × sku. expiry spread ~today..+2y, with a
-- slice already inside 90 days so near_expiry_stock > 0.
INSERT INTO inventory (store_id, sku, qty, expiry)
SELECT s.store_id, d.sku,
  floor(random()*300)::int,
  current_date + (floor(random()*760) - 30)::int * interval '1 day'
FROM stores s CROSS JOIN drugs d;

-- sales grain: revenue (amount) + COGS (cost_amount) denormalized on the row;
-- cost ratio by category so margin_rate differs across 品类. is_rx mirrors the drug.
INSERT INTO sales (sale_id, store_id, sku, member_id, sold_at, qty, amount, cost_amount, is_rx)
SELECT s.g, s.store_id, s.sku, s.member_id, s.sold_at, s.qty,
  round((s.qty*d.base_price*(0.9+random()*0.15))::numeric,2),
  round((s.qty*d.base_price*(CASE d.category
    WHEN '处方药' THEN 0.75 WHEN 'OTC' THEN 0.60 WHEN '中成药' THEN 0.55 ELSE 0.40 END))::numeric,2),
  d.is_rx
FROM (
  SELECT g, 1+floor(random()*15)::int AS store_id, 1+floor(random()*1200)::int AS sku,
    CASE WHEN random()<0.6 THEN 1+floor(random()*40000)::int ELSE NULL END AS member_id,
    date '2025-07-01' + (random()*364)*interval '1 day' AS sold_at,
    1+floor(random()*4)::int AS qty
  FROM generate_series(1,150000) g
) s JOIN drugs d ON d.sku = s.sku;
