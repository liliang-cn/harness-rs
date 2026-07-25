-- Manufacturing quality: production grain (work orders) vs inspection grain
-- vs defect grain (defects fan out per inspection). defect_rate crosses defect
-- and inspection grains -> only DI's per-grain CTEs stay chasm-safe.
-- revenue / cost / gross_margin finance-gated; operator phone masked.
DROP TABLE IF EXISTS downtime, defects, inspections, work_orders, products, lines, plants CASCADE;

CREATE TABLE plants (plant_id int PRIMARY KEY, name text, region text);
CREATE TABLE lines (line_id int PRIMARY KEY, plant_id int REFERENCES plants, name text);
CREATE TABLE products (product_id int PRIMARY KEY, name text, category text, unit_cost numeric);
CREATE TABLE work_orders (
  wo_id int PRIMARY KEY, plant_id int REFERENCES plants, line_id int REFERENCES lines,
  product_id int REFERENCES products, operator_phone text, started_at date,
  produced_qty int, scrapped_qty int, good_qty int,
  revenue_amount numeric, cost_amount numeric, status text);
CREATE TABLE inspections (
  insp_id int PRIMARY KEY, plant_id int REFERENCES plants, line_id int REFERENCES lines,
  wo_id int REFERENCES work_orders, inspected_at date, inspected_qty int, passed_qty int);
CREATE TABLE defects (
  defect_id int PRIMARY KEY, insp_id int REFERENCES inspections, plant_id int REFERENCES plants,
  line_id int REFERENCES lines, defect_type text, severity text, qty int);
CREATE TABLE downtime (
  down_id int PRIMARY KEY, plant_id int REFERENCES plants, line_id int REFERENCES lines,
  event_at date, minutes int, reason text);

SELECT setseed(0.51);

INSERT INTO plants VALUES
  (1,'华东制造基地·苏州','华东'),(2,'华南制造基地·东莞','华南'),
  (3,'华北制造基地·天津','华北'),(4,'西南制造基地·重庆','西南');

-- 12 production lines, 3 per plant.
INSERT INTO lines (line_id, plant_id, name)
SELECT g, 1+((g-1)/3), '产线-'||g FROM generate_series(1,12) g;

-- 150 products across 5 categories, unit_cost by category.
INSERT INTO products (product_id, name, category, unit_cost)
SELECT g, '型号-'||g,
  (ARRAY['结构件','电子件','注塑件','紧固件','总成'])[1+(g%5)],
  round(((ARRAY[45,120,18,6,260])[1+(g%5)] * (0.8+random()*0.5))::numeric, 2)
FROM generate_series(1,150) g;

-- 20k work orders (production grain). scrap 2-10%; good = produced - scrapped;
-- revenue = good*price, cost = produced*unit_cost denormalized. ~95% completed.
INSERT INTO work_orders (wo_id, plant_id, line_id, product_id, operator_phone, started_at,
                         produced_qty, scrapped_qty, good_qty, revenue_amount, cost_amount, status)
SELECT s.g, ln.plant_id, s.line_id, p.product_id,
  CASE WHEN random()<0.3
    THEN '1'||(3+floor(random()*7))::int||lpad(floor(random()*1000000000)::text,9,'0')
    ELSE NULL END,
  s.started, s.pq, s.scq, s.pq - s.scq,
  round(((s.pq - s.scq) * p.unit_cost * (1.4+random()*0.4))::numeric, 2),   -- revenue
  round((s.pq * p.unit_cost)::numeric, 2),                                   -- cost @ fact grain
  CASE WHEN s.u < 0.95 THEN 'completed' ELSE 'in_progress' END
FROM (
  SELECT g, 1+floor(random()*12)::int AS line_id, 1+floor(random()*150)::int AS product_ix,
    (date '2025-01-01' + (floor(random()*365))::int * interval '1 day')::date AS started,
    pq, floor(pq * (0.02+random()*0.08))::int AS scq, random() AS u
  FROM (SELECT g, (ARRAY[200,400,600,800,1000])[1+floor(random()*5)::int] AS pq
        FROM generate_series(1,20000) g) q
) s
JOIN lines ln ON ln.line_id = s.line_id
JOIN products p ON p.product_id = s.product_ix;

-- Inspection grain: one inspection per work order. passed 90-99% of inspected.
INSERT INTO inspections (insp_id, plant_id, line_id, wo_id, inspected_at, inspected_qty, passed_qty)
SELECT w.wo_id, w.plant_id, w.line_id, w.wo_id, w.started_at, w.produced_qty,
  floor(w.produced_qty * (0.90 + random()*0.09))::int
FROM work_orders w;

-- Defect grain: 1-3 defect rows per inspection that had failures (fans out).
INSERT INTO defects (defect_id, insp_id, plant_id, line_id, defect_type, severity, qty)
SELECT row_number() OVER (), i.insp_id, i.plant_id, i.line_id,
  (ARRAY['尺寸偏差','外观划痕','装配不良','焊接缺陷','功能失效'])[1+((i.insp_id + d.k) % 5)],
  (ARRAY['轻微','一般','严重'])[1+((i.insp_id * d.k) % 3)],
  1 + ((i.insp_id * d.k) % 5)
FROM inspections i
JOIN LATERAL generate_series(1, 1 + (i.insp_id % 3)) AS d(k)
  ON i.passed_qty < i.inspected_qty;

-- 4k downtime events.
INSERT INTO downtime (down_id, plant_id, line_id, event_at, minutes, reason)
SELECT s.g, ln.plant_id, s.line_id,
  (date '2025-01-01' + (floor(random()*365))::int * interval '1 day')::date,
  (10+floor(random()*180))::int,
  (ARRAY['换型','故障','缺料','保养','质量停机'])[1+floor(random()*5)::int]
FROM (SELECT g, 1+floor(random()*12)::int AS line_id FROM generate_series(1,4000) g) s
JOIN lines ln ON ln.line_id = s.line_id;
