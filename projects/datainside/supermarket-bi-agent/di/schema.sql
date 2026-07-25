-- Larger supermarket: many SKUs, ~200k sales. sales grain vs inventory grain
-- -> sell_through is chasm-safe. gross_margin/revenue are finance-gated.
DROP TABLE IF EXISTS sales, inventory, products, members, departments CASCADE;
CREATE TABLE departments (dept_id int PRIMARY KEY, name text);
CREATE TABLE products (sku int PRIMARY KEY, name text, dept_id int REFERENCES departments, category text, price numeric, cost numeric);
CREATE TABLE members (member_id int PRIMARY KEY, phone text, tier text);
CREATE TABLE inventory (sku int PRIMARY KEY REFERENCES products, qty int, days_in_stock int);
CREATE TABLE sales (
  sale_id bigint PRIMARY KEY, sku int REFERENCES products, member_id int REFERENCES members,
  sold_at timestamp, qty int, amount numeric);

SELECT setseed(0.37);
INSERT INTO departments VALUES (1,'生鲜'),(2,'粮油'),(3,'日配'),(4,'休食'),(5,'日用'),(6,'家电');
INSERT INTO products
SELECT g, '商品-'||g, 1+(g%6),
  (ARRAY['蔬果','肉禽','水产','米面','食用油','乳品','熟食','饼干','饮料','清洁','纸品','小家电'])[1+(g%12)],
  round((5 + random()*2000 * (1+(g%6))/3.0)::numeric,2) AS price,
  0
FROM generate_series(1,1500) g;
UPDATE products SET cost = round((price*(0.6+random()*0.25))::numeric,2);
INSERT INTO members
SELECT g, '1'||(3+floor(random()*7))::int||lpad(floor(random()*1000000000)::text,9,'0'),
  (ARRAY['普通','普通','银卡','金卡'])[1+(g%4)]
FROM generate_series(1,5000) g;
INSERT INTO inventory
SELECT sku, floor(random()*200)::int, floor(random()*120)::int FROM products;

INSERT INTO sales (sale_id, sku, member_id, sold_at, qty, amount)
SELECT s.g, s.sku, s.member_id, s.sold_at, s.qty, round((s.qty*p.price*(0.9+random()*0.15))::numeric,2)
FROM (
  SELECT g, 1+floor(random()*1500)::int AS sku,
    CASE WHEN random()<0.6 THEN 1+floor(random()*5000)::int ELSE NULL END AS member_id,
    date '2025-07-01' + (random()*364)*interval '1 day' AS sold_at,
    1+floor(random()*4)::int AS qty
  FROM generate_series(1,200000) g
) s JOIN products p ON p.sku = s.sku;
