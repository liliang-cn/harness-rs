-- 汽修连锁: work-order grain vs labor-capacity grain -> labor_utilization is a
-- chasm metric (DI sums each in its own CTE). parts revenue/margin finance-gated,
-- rework_rate at wo grain, customer phone is PII.
DROP TABLE IF EXISTS parts_lines, labor_capacity, work_orders, customers, technicians, shops CASCADE;
CREATE TABLE shops (shop_id int PRIMARY KEY, name text, city text);
CREATE TABLE technicians (tech_id int PRIMARY KEY, name text, shop_id int REFERENCES shops);
CREATE TABLE customers (customer_id int PRIMARY KEY, name text, phone text, plate text);
CREATE TABLE work_orders (
  wo_id int PRIMARY KEY, shop_id int REFERENCES shops, tech_id int REFERENCES technicians,
  customer_id int REFERENCES customers, opened_at timestamp, status text,
  labor_hours numeric, labor_amount numeric, is_rework bool);
CREATE TABLE parts_lines (
  line_id bigint PRIMARY KEY, wo_id int REFERENCES work_orders, part_name text,
  category text, qty int, amount numeric, cost_amount numeric);
CREATE TABLE labor_capacity (
  cap_id bigint PRIMARY KEY, shop_id int REFERENCES shops, day date, available_labor_hours numeric);

SELECT setseed(0.42);
INSERT INTO shops (shop_id, name, city) VALUES
  (1,'中心旗舰店','北京'),(2,'朝阳快修店','北京'),
  (3,'浦东旗舰店','上海'),(4,'徐汇快修店','上海'),
  (5,'天河旗舰店','广州'),(6,'番禺快修店','广州'),
  (7,'锦江快修店','成都'),(8,'西湖快修店','杭州'),
  (9,'南山快修店','深圳'),(10,'江汉快修店','武汉');
-- 80 技师均匀铺到 10 家店 (每店 8 人 -> 每店产能一致)
INSERT INTO technicians
SELECT g, '技师-'||g, 1+(g%10) FROM generate_series(1,80) g;
-- 2.5 万车主, ~30% 留了手机号 (PII), 其余 NULL; 车牌如 京A12345
INSERT INTO customers (customer_id, name, phone, plate)
SELECT g, '车主-'||g,
  CASE WHEN random()<0.30
       THEN '1'||(3+floor(random()*7))::int||lpad(floor(random()*1000000000)::text,9,'0')
       ELSE NULL END,
  (ARRAY['京','沪','粤','川','鲁','浙'])[1+(g%6)]||(ARRAY['A','B','C','D'])[1+(g%4)]||lpad((g%100000)::text,5,'0')
FROM generate_series(1,25000) g;

-- 4 万工单. 一次抽签 r 定状态 completed88%/redo7%(=返修)/open5%; 工时 0.5-6h,
-- labor_amount = 工时 x 工时费率 (整单行, 无需 join)
INSERT INTO work_orders (wo_id, shop_id, tech_id, customer_id, opened_at, status, labor_hours, labor_amount, is_rework)
SELECT b.g, b.shop_id, b.tech_id, b.customer_id, b.opened_at,
  CASE WHEN b.r<0.88 THEN 'completed' WHEN b.r<0.95 THEN 'redo' ELSE 'open' END AS status,
  b.labor_hours,
  round((b.labor_hours*b.rate)::numeric,2) AS labor_amount,
  (b.r>=0.88 AND b.r<0.95) AS is_rework
FROM (
  SELECT g,
    1+floor(random()*10)::int AS shop_id,
    1+floor(random()*80)::int AS tech_id,
    1+floor(random()*25000)::int AS customer_id,
    date '2025-07-01' + (floor(random()*365))::int * interval '1 day'
      + (8+floor(random()*10))::int * interval '1 hour'
      + (floor(random()*60))::int * interval '1 minute' AS opened_at,
    random() AS r,
    round((0.5+random()*5.5)::numeric,1) AS labor_hours,
    (280+random()*140) AS rate
  FROM generate_series(1,40000) g
) b;

-- 配件行, 1-4 条/工单 (关联 wo_id, 避免非关联 LATERAL 只物化一次). cost_amount
-- 冗余落到行上 -> 毛利无需 join. 单价按品类随机, 成本率 0.55-0.8
INSERT INTO parts_lines (line_id, wo_id, part_name, category, qty, amount, cost_amount)
SELECT row_number() OVER () AS line_id, w.wo_id,
  cat.category||'-'||w.wo_id AS part_name, cat.category, cat.qty,
  round((cat.qty*cat.unit)::numeric,2) AS amount,
  round((cat.qty*cat.unit*(0.55+random()*0.25))::numeric,2) AS cost_amount
FROM work_orders w
CROSS JOIN LATERAL (
  SELECT (ARRAY['机油','轮胎','刹车','电瓶','滤芯','其他'])[1+floor(random()*6)::int] AS category,
    1+floor(random()*4)::int AS qty,
    (30+random()*800) AS unit
  FROM generate_series(1, 1+(w.wo_id%4)) gs
) cat;

-- 产能行: 每店每天 = 技师数 x ~7.25 个可用工时 (含休息/内务, 故工时利用率 <100%).
-- 与工单不同粒度 -> labor_utilization 是 chasm 指标
INSERT INTO labor_capacity (cap_id, shop_id, day, available_labor_hours)
SELECT row_number() OVER () AS cap_id, s.shop_id, d.day,
  round((tc.n*(6.5+random()*1.5))::numeric,1) AS available_labor_hours
FROM shops s
JOIN (SELECT shop_id, count(*) n FROM technicians GROUP BY shop_id) tc ON tc.shop_id = s.shop_id
CROSS JOIN generate_series(date '2025-07-01', date '2026-06-30', interval '1 day') d(day);
