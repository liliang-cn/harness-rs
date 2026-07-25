-- Forging company: 2 factories, 15 production lines. production_runs (per line/shift)
-- and inspections/defects live at different grains -> defect_rate is chasm-safe.
DROP TABLE IF EXISTS defects, inspections, production_runs, parts, lines, factories CASCADE;
CREATE TABLE factories (factory_id int PRIMARY KEY, name text, region text);
CREATE TABLE lines (line_id int PRIMARY KEY, factory_id int REFERENCES factories, name text, press_tonnage int);
CREATE TABLE parts (part_id int PRIMARY KEY, name text, material_grade text, target_weight_kg numeric);
CREATE TABLE production_runs (
  run_id bigint PRIMARY KEY, line_id int REFERENCES lines, part_id int REFERENCES parts,
  run_date date, shift text, output_units int, good_units int, downtime_min int, tonnes numeric);
CREATE TABLE inspections (
  inspection_id bigint PRIMARY KEY, run_id bigint REFERENCES production_runs, sampled int);
CREATE TABLE defects (
  defect_id bigint PRIMARY KEY, inspection_id bigint REFERENCES inspections, defect_type text);

SELECT setseed(0.19);
INSERT INTO factories VALUES (1,'华东锻造一厂','East'),(2,'西南锻造二厂','West');

-- 15 lines: 8 in factory 1, 7 in factory 2
INSERT INTO lines
SELECT g, CASE WHEN g<=8 THEN 1 ELSE 2 END, '产线-'||g,
  (ARRAY[1600,2500,4000,6300,8000])[1+(g%5)]
FROM generate_series(1,15) g;

INSERT INTO parts
SELECT g,
  (ARRAY['法兰盘','齿轮坯','曲轴','连杆','转向节','轮毂','传动轴'])[1+(g%7)]||'-'||g,
  (ARRAY['42CrMo','20CrMnTi','40Cr','35CrMo','16MnCr5'])[1+(g%5)],
  round((2 + random()*18)::numeric,1)
FROM generate_series(1,25) g;

-- production runs: 15 lines x ~180 days x 2 shifts
INSERT INTO production_runs (run_id, line_id, part_id, run_date, shift, output_units, good_units, downtime_min, tonnes)
SELECT row_number() over (),
  l.line_id, 1+floor(random()*25)::int, d, sh.shift,
  o.output_units,
  greatest(0, o.output_units - floor(o.output_units*(0.02+random()*0.08))::int),  -- 2-10% scrap
  floor(random()*90)::int,
  round((o.output_units * (2+random()*18) / 1000.0)::numeric,2)
FROM lines l
CROSS JOIN generate_series(date '2025-12-01', date '2026-05-29', interval '1 day') d
CROSS JOIN (VALUES ('day'),('night')) sh(shift)
CROSS JOIN LATERAL (SELECT (200 + floor(random()*600))::int AS output_units) o;

-- one inspection per run, sampling 20-60 units
INSERT INTO inspections (inspection_id, run_id, sampled)
SELECT run_id, run_id, (20+floor(random()*40))::int FROM production_runs;

-- defects: per inspection, 0-6 defects with a type
INSERT INTO defects (defect_id, inspection_id, defect_type)
SELECT row_number() over (), i.inspection_id,
  (ARRAY['折叠','裂纹','欠压','夹杂','脱碳','尺寸超差'])[1+floor(random()*6)::int]
FROM inspections i
CROSS JOIN LATERAL generate_series(1, floor(random()*7)::int) x;

-- ===== business expansion: sales, R&D cost, production scheduling =====
DROP TABLE IF EXISTS rd_costs, rd_projects, sales_orders, shift_plans, customers CASCADE;
CREATE TABLE customers (customer_id int PRIMARY KEY, name text, region text, tier text);
CREATE TABLE sales_orders (
  order_id bigint PRIMARY KEY, customer_id int REFERENCES customers, part_id int REFERENCES parts,
  order_date date, qty int, amount numeric, promised_date date, shipped_date date);
CREATE TABLE rd_projects (project_id int PRIMARY KEY, name text, part_id int REFERENCES parts, status text, start_date date);
CREATE TABLE rd_costs (
  cost_id bigint PRIMARY KEY, project_id int REFERENCES rd_projects, cost_date date, category text, amount numeric);
CREATE TABLE shift_plans (
  plan_id bigint PRIMARY KEY, line_id int REFERENCES lines, plan_date date, shift text,
  planned_output int, headcount int, planned_hours numeric);

SELECT setseed(0.23);
INSERT INTO customers
SELECT g, (ARRAY['一汽','东风','上汽','广汽','比亚迪','吉利','长城','潍柴','康明斯','采埃孚','博世','舍弗勒'])[g]||'配套',
  (ARRAY['East','West','Central'])[1+(g%3)], (ARRAY['A','A','B','C'])[1+(g%4)]
FROM generate_series(1,12) g;

INSERT INTO sales_orders (order_id, customer_id, part_id, order_date, qty, amount, promised_date, shipped_date)
SELECT s.g, s.customer_id, s.part_id, s.order_date, s.qty,
  round((s.qty * (100 + random()*400))::numeric,2),
  s.order_date + (20+floor(random()*20)::int),
  s.order_date + (20+floor(random()*20)::int) + (CASE WHEN random()<0.85 THEN -floor(random()*5)::int ELSE floor(random()*12)::int END)
FROM (
  SELECT g, 1+floor(random()*12)::int AS customer_id, 1+floor(random()*25)::int AS part_id,
    date '2025-12-01' + (random()*180)::int AS order_date, (50+floor(random()*450))::int AS qty
  FROM generate_series(1,8000) g
) s;

INSERT INTO rd_projects
SELECT g, '研发项目-'||(ARRAY['轻量化','高强钢','近净成形','工艺降本','新客户试制'])[1+(g%5)]||'-'||g,
  1+floor(random()*25)::int, (ARRAY['active','active','completed','on_hold'])[1+(g%4)],
  date '2025-06-01' + (random()*300)::int
FROM generate_series(1,15) g;

INSERT INTO rd_costs (cost_id, project_id, cost_date, category, amount)
SELECT row_number() over (), p.project_id,
  date '2025-12-01' + (random()*180)::int,
  (ARRAY['人力','材料','试制','设备'])[1+floor(random()*4)::int],
  round((5000 + random()*80000)::numeric,2)
FROM rd_projects p
CROSS JOIN LATERAL generate_series(1, (30+floor(random()*120))::int) x;

INSERT INTO shift_plans (plan_id, line_id, plan_date, shift, planned_output, headcount, planned_hours)
SELECT row_number() over (), l.line_id, d, sh.shift,
  (400 + floor(random()*500))::int, (4+floor(random()*6))::int, round((8+random()*4)::numeric,1)
FROM lines l
CROSS JOIN generate_series(date '2025-12-01', date '2026-05-29', interval '1 day') d
CROSS JOIN (VALUES ('day'),('night')) sh(shift);
