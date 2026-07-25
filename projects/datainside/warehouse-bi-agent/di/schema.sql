-- Warehouse / WMS: flow grain (outbound order lines) vs snapshot grain
-- (daily on-hand). inventory_turnover crosses both -> only DI's per-grain CTEs
-- stay chasm-safe. revenue / cost / gross_margin finance-gated; customer phone masked.
DROP TABLE IF EXISTS pick_tasks, inventory_snapshot, order_lines, orders,
                     inbound_lines, skus, warehouses CASCADE;

CREATE TABLE warehouses (warehouse_id int PRIMARY KEY, name text, city text, region text);
CREATE TABLE skus (sku_id int PRIMARY KEY, name text, category text, unit_cost numeric);
CREATE TABLE inbound_lines (
  inbound_id int PRIMARY KEY, warehouse_id int REFERENCES warehouses, sku_id int REFERENCES skus,
  received_at date, qty int, status text);
CREATE TABLE orders (
  order_id int PRIMARY KEY, warehouse_id int REFERENCES warehouses,
  customer text, contact_phone text, ordered_at date);
CREATE TABLE order_lines (
  line_id int PRIMARY KEY, order_id int REFERENCES orders, warehouse_id int REFERENCES warehouses,
  sku_id int REFERENCES skus, shipped_at date, ordered_qty int, shipped_qty int,
  revenue_amount numeric, cost_amount numeric, status text);
CREATE TABLE inventory_snapshot (
  snap_id int PRIMARY KEY, warehouse_id int REFERENCES warehouses, sku_id int REFERENCES skus,
  day date, on_hand_qty int);
CREATE TABLE pick_tasks (
  pick_id int PRIMARY KEY, warehouse_id int REFERENCES warehouses, order_id int REFERENCES orders,
  assigned_at date, sla_minutes int, actual_minutes int, on_time int);

SELECT setseed(0.37);

INSERT INTO warehouses VALUES
  (1,'华北中心仓·北京','北京','华北'),(2,'华东中心仓·上海','上海','华东'),
  (3,'华南中心仓·广州','广州','华南'),(4,'华中区域仓·武汉','武汉','华中'),
  (5,'西南区域仓·成都','成都','西南'),(6,'华东区域仓·杭州','杭州','华东');

-- 200 SKUs across 6 categories, unit_cost by category.
INSERT INTO skus (sku_id, name, category, unit_cost)
SELECT g, 'SKU-'||g,
  (ARRAY['3C数码','家居','美妆','食品','服饰','母婴'])[1+(g%6)],
  round(((ARRAY[220,80,120,25,60,90])[1+(g%6)] * (0.8+random()*0.5))::numeric, 2)
FROM generate_series(1,200) g;

-- 20k inbound (putaway) lines; ~92% putaway_done.
INSERT INTO inbound_lines (inbound_id, warehouse_id, sku_id, received_at, qty, status)
SELECT g, 1+floor(random()*6)::int, 1+floor(random()*200)::int,
  (date '2025-01-01' + (floor(random()*365))::int * interval '1 day')::date,
  (ARRAY[40,60,80,120,200])[1+floor(random()*5)::int],
  CASE WHEN random()<0.92 THEN 'putaway_done' ELSE 'in_receiving' END
FROM generate_series(1,20000) g;

-- 15k orders; valid CN mobile in ~30% else NULL (PII for the mask story).
INSERT INTO orders (order_id, warehouse_id, customer, contact_phone, ordered_at)
SELECT g, 1+floor(random()*6)::int, '客户-'||(1+floor(random()*3000)::int),
  CASE WHEN random()<0.3
    THEN '1'||(3+floor(random()*7))::int||lpad(floor(random()*1000000000)::text,9,'0')
    ELSE NULL END,
  (date '2025-01-01' + (floor(random()*365))::int * interval '1 day')::date
FROM generate_series(1,15000) g;

-- 40k outbound order lines (flow grain). shipped_qty <= ordered_qty (fill rate
-- story); revenue = shipped*price, cost = shipped*unit_cost denormalized so
-- gross_margin needs no join. status 90% shipped.
INSERT INTO order_lines (line_id, order_id, warehouse_id, sku_id, shipped_at,
                         ordered_qty, shipped_qty, revenue_amount, cost_amount, status)
SELECT s.g, o.order_id, o.warehouse_id, sk.sku_id,
  (o.ordered_at + (1+floor(random()*4))::int * interval '1 day')::date,
  s.oq, fill.sq,
  round((fill.sq * sk.unit_cost * (1.35+random()*0.5))::numeric, 2),   -- revenue
  round((fill.sq * sk.unit_cost)::numeric, 2),                          -- cost @ fact grain
  CASE WHEN s.u < 0.90 THEN 'shipped' ELSE 'backordered' END
FROM (
  SELECT g, 1+floor(random()*15000)::int AS order_id, 1+floor(random()*200)::int AS sku_ix,
    (ARRAY[2,4,6,10,20])[1+floor(random()*5)::int] AS oq,
    random() AS r, random() AS u
  FROM generate_series(1,40000) g
) s
JOIN orders o ON o.order_id = s.order_id
JOIN skus sk ON sk.sku_id = s.sku_ix
CROSS JOIN LATERAL (SELECT CASE WHEN s.r < 0.8 THEN s.oq
                                WHEN s.r < 0.95 THEN greatest(s.oq-2,0)
                                ELSE 0 END AS sq) fill;

-- Snapshot grain: warehouse × sku × month-end (12 months). on_hand correlated
-- to sku so turnover varies. ~14.4k rows.
INSERT INTO inventory_snapshot (snap_id, warehouse_id, sku_id, day, on_hand_qty)
SELECT row_number() OVER (), w.warehouse_id, sk.sku_id,
  (date '2025-01-31' + (m.mo-1) * interval '1 month')::date,
  greatest(20, (200 + (sk.sku_id % 40)*15 - m.mo*3 + floor(random()*120))::int)
FROM warehouses w
CROSS JOIN skus sk
CROSS JOIN generate_series(1,12) AS m(mo);

-- 15k pick tasks (pick grain); SLA 120 min, ~85% on time.
INSERT INTO pick_tasks (pick_id, warehouse_id, order_id, assigned_at, sla_minutes, actual_minutes, on_time)
SELECT s.g, o.warehouse_id, o.order_id, o.ordered_at, 120, s.am,
  CASE WHEN s.am <= 120 THEN 1 ELSE 0 END
FROM (
  SELECT g, 1+floor(random()*15000)::int AS order_id,
    CASE WHEN random()<0.85 THEN (30+floor(random()*90))::int ELSE (121+floor(random()*120))::int END AS am
  FROM generate_series(1,15000) g
) s JOIN orders o ON o.order_id = s.order_id;
