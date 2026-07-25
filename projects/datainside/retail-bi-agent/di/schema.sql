-- Appliance-retail warehouse for the retail-bi vertical's DI semantic model.
DROP TABLE IF EXISTS sales, inventory, products, stores CASCADE;
CREATE TABLE stores    (store_id int PRIMARY KEY, name text, city text, region text, opened_at date);
CREATE TABLE products  (sku text PRIMARY KEY, name text, category text, price numeric);
CREATE TABLE inventory (store_id int REFERENCES stores, sku text REFERENCES products, qty int, days_in_stock int);
CREATE TABLE sales (
  sale_id bigint PRIMARY KEY, store_id int REFERENCES stores, sku text REFERENCES products,
  qty int, amount numeric, member_phone text, is_trade_in boolean, sold_at timestamp);

SELECT setseed(0.31);
INSERT INTO stores VALUES
 (1,'城东旗舰店','成都','West','2020-05-01'),
 (2,'城西店','成都','West','2021-09-10'),
 (3,'高新店','成都','West','2022-03-15'),
 (4,'重庆解放碑店','重庆','Central','2021-01-20'),
 (5,'西安钟楼店','西安','Central','2022-07-01');

INSERT INTO products
SELECT 'SKU-'||g,
  (ARRAY['格力','美的','海尔','LG','西门子','小米','TCL','海信'])[1+(g%8)]||(ARRAY['空调','冰箱','洗衣机','电视','厨电'])[1+(g%5)]||'-'||g,
  (ARRAY['空调','冰箱','洗衣机','电视','厨电'])[1+(g%5)],
  round(((ARRAY[3299,3999,3599,4999,1999])[1+(g%5)] * (0.8+random()*0.6))::numeric,0)
FROM generate_series(1,40) g;

INSERT INTO inventory
SELECT s.store_id, p.sku, floor(random()*30)::int, floor(random()*180)::int
FROM stores s CROSS JOIN products p;

INSERT INTO sales (sale_id, store_id, sku, qty, amount, member_phone, is_trade_in, sold_at)
SELECT g, 1+floor(random()*5)::int, p.sku, q.qty,
  round((q.qty * p.price * (0.85+random()*0.2))::numeric,2),
  CASE WHEN random()<0.3 THEN '1'||(3+floor(random()*7))::int||lpad(floor(random()*1000000000)::text,9,'0') ELSE NULL END,
  random()<0.15,
  date '2025-07-01' + (random()*364)*interval '1 day'
FROM generate_series(1,50000) g
CROSS JOIN LATERAL (SELECT sku, price FROM products ORDER BY random() LIMIT 1) p
CROSS JOIN LATERAL (SELECT 1+floor(random()*3)::int AS qty) q;
