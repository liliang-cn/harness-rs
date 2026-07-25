-- E-commerce BI warehouse schema and realistic synthetic data generator.
DROP TABLE IF EXISTS reviews CASCADE;
DROP TABLE IF EXISTS order_items CASCADE;
DROP TABLE IF EXISTS orders CASCADE;
DROP TABLE IF EXISTS customers CASCADE;
DROP TABLE IF EXISTS products CASCADE;

CREATE TABLE products (
  sku text PRIMARY KEY,
  name text NOT NULL,
  category text NOT NULL,
  brand text NOT NULL,
  unit_price numeric(12, 2) NOT NULL,
  unit_cost numeric(12, 2) NOT NULL,
  stock_qty int NOT NULL
);

CREATE TABLE customers (
  customer_id int PRIMARY KEY,
  name text NOT NULL,
  city text NOT NULL,
  province text NOT NULL,
  tier text NOT NULL
);

CREATE TABLE orders (
  order_id bigint PRIMARY KEY,
  order_no text NOT NULL UNIQUE,
  customer_id int REFERENCES customers(customer_id),
  status text NOT NULL,
  channel text NOT NULL,
  total_amount numeric(12, 2) NOT NULL,
  total_cost numeric(12, 2) NOT NULL,
  ordered_at timestamp NOT NULL
);

CREATE TABLE order_items (
  item_id bigint PRIMARY KEY,
  order_id bigint REFERENCES orders(order_id),
  sku text REFERENCES products(sku),
  qty int NOT NULL,
  unit_price numeric(12, 2) NOT NULL,
  amount numeric(12, 2) NOT NULL,
  cost_amount numeric(12, 2) NOT NULL
);

CREATE TABLE reviews (
  review_id bigint PRIMARY KEY,
  sku text REFERENCES products(sku),
  customer_id int REFERENCES customers(customer_id),
  rating int NOT NULL,
  comment text NOT NULL,
  created_at timestamp NOT NULL
);

SELECT setseed(0.2026);

-- 1. Insert Products (100 SKUs across 8 Categories & 16 Brands)
INSERT INTO products
SELECT 
  'ECOM-SKU-' || lpad(g::text, 4, '0'),
  (ARRAY['华为', '苹果', '小米', '联想', '安踏', '李宁', '雅诗兰黛', '欧莱雅', '三只松鼠', '百草味', '美的', '海尔', '戴森', '波司登', '全棉时代', '罗技'])[1 + (g % 16)] ||
  (ARRAY['数码手机', '智能手表', '无线耳机', '运动鞋', '保暖羽绒服', '修护精华', '保湿乳液', '坚果礼盒', '扫地机器人', '空气净化器'])[1 + (g % 10)] || '-' || g,
  (ARRAY['数码3C', '智能穿戴', '鞋服箱包', '美妆护肤', '休闲食品', '生活家电', '母婴用品', '运动户外'])[1 + (g % 8)],
  (ARRAY['华为', '苹果', '小米', '联想', '安踏', '李宁', '雅诗兰黛', '欧莱雅', '三只松鼠', '百草味', '美的', '海尔', '戴森', '波司登', '全棉时代', '罗技'])[1 + (g % 16)],
  round(((ARRAY[199, 499, 899, 1299, 2499, 3999, 5999, 8999])[1 + (g % 8)] * (0.85 + random() * 0.3))::numeric, 2),
  round(((ARRAY[199, 499, 899, 1299, 2499, 3999, 5999, 8999])[1 + (g % 8)] * (0.45 + random() * 0.2))::numeric, 2),
  floor(10 + random() * 500)::int
FROM generate_series(1, 100) g;

-- 2. Insert Customers (1,000 Users)
INSERT INTO customers
SELECT 
  g,
  (ARRAY['张', '王', '李', '赵', '陈', '刘', '杨', '黄', '周', '吴'])[1 + (g % 10)] ||
  (ARRAY['伟', '芳', '娜', '秀英', '敏', '静', '丽', '强', '磊', '洋', '艳', '勇', '军', '杰', '娟'])[1 + (g % 15)],
  (ARRAY['北京', '上海', '广州', '深圳', '杭州', '成都', '武汉', '西安', '南京', '重庆'])[1 + (g % 10)],
  (ARRAY['北京', '上海', '广东', '广东', '浙江', '四川', '湖北', '陕西', '江苏', '重庆'])[1 + (g % 10)],
  (ARRAY['注册用户', '黄金会员', '铂金会员', '钻石会员', 'VIP尊享会员'])[1 + (g % 5)]
FROM generate_series(1, 1000) g;

-- 3. Insert Orders (30,000 Orders)
WITH raw_orders AS (
  SELECT 
    g AS order_id,
    'ORD2025' || lpad(g::text, 8, '0') AS order_no,
    1 + floor(random() * 1000)::int AS customer_id,
    (ARRAY['已完成', '已完成', '已完成', '已完成', '已退款', '已取消'])[1 + floor(random() * 6)::int] AS status,
    (ARRAY['天猫旗舰店', '京东自营', '抖音直播间', '微信小程序', 'App自营店'])[1 + floor(random() * 5)::int] AS channel,
    timestamp '2025-01-01 00:00:00' + (random() * 365) * interval '1 day' AS ordered_at
  FROM generate_series(1, 30000) g
),
items_gen AS (
  SELECT 
    row_number() OVER () AS item_id,
    o.order_id,
    p.sku,
    (1 + floor(random() * 3))::int AS qty,
    p.unit_price,
    p.unit_cost
  FROM raw_orders o
  CROSS JOIN LATERAL (
    SELECT sku, unit_price, unit_cost FROM products ORDER BY random() LIMIT (1 + floor(random() * 2))::int
  ) p
),
items_calc AS (
  SELECT 
    item_id,
    order_id,
    sku,
    qty,
    unit_price,
    round((qty * unit_price)::numeric, 2) AS amount,
    round((qty * unit_cost)::numeric, 2) AS cost_amount
  FROM items_gen
),
orders_agg AS (
  SELECT 
    order_id,
    round(sum(amount)::numeric, 2) AS total_amount,
    round(sum(cost_amount)::numeric, 2) AS total_cost
  FROM items_calc
  GROUP BY order_id
)
INSERT INTO orders (order_id, order_no, customer_id, status, channel, total_amount, total_cost, ordered_at)
SELECT 
  ro.order_id,
  ro.order_no,
  ro.customer_id,
  ro.status,
  ro.channel,
  oa.total_amount,
  oa.total_cost,
  ro.ordered_at
FROM raw_orders ro
JOIN orders_agg oa ON ro.order_id = oa.order_id;

-- 4. Insert Order Items
INSERT INTO order_items (item_id, order_id, sku, qty, unit_price, amount, cost_amount)
WITH items_gen AS (
  SELECT 
    row_number() OVER () AS item_id,
    o.order_id,
    p.sku,
    (1 + floor(random() * 3))::int AS qty,
    p.unit_price,
    p.unit_cost
  FROM orders o
  CROSS JOIN LATERAL (
    SELECT sku, unit_price, unit_cost FROM products ORDER BY random() LIMIT (1 + floor(random() * 2))::int
  ) p
)
SELECT 
  item_id,
  order_id,
  sku,
  qty,
  unit_price,
  round((qty * unit_price)::numeric, 2) AS amount,
  round((qty * unit_cost)::numeric, 2) AS cost_amount
FROM items_gen;

-- 5. Insert Product Reviews (10,000 Reviews)
INSERT INTO reviews (review_id, sku, customer_id, rating, comment, created_at)
SELECT 
  g,
  p.sku,
  1 + floor(random() * 1000)::int,
  (ARRAY[5, 5, 5, 4, 4, 3, 2, 1])[1 + floor(random() * 8)::int],
  (ARRAY['质量非常好，物流很快！', '包装精美，性价比高，推荐购买。', '做工不错，符合预期。', '整体还可以，稍微有一点小缺点。', '体验一般，发货有点慢。', '商品有磕碰，服务态度需要改进。'])[1 + floor(random() * 6)::int],
  timestamp '2025-01-05 00:00:00' + (random() * 360) * interval '1 day'
FROM generate_series(1, 10000) g
CROSS JOIN LATERAL (
  SELECT sku FROM products ORDER BY random() LIMIT 1
) p;
