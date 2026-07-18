-- Teahouse (茶馆): orders (per sitting) and order_items (per tea) are different
-- grains, so items_per_order is chasm-safe. member_phone is masked PII.
DROP TABLE IF EXISTS order_items, orders, teas, staff, rooms CASCADE;
CREATE TABLE rooms (room_id int PRIMARY KEY, name text, room_type text, seats int);
CREATE TABLE teas  (tea_id int PRIMARY KEY, name text, category text, price numeric);
CREATE TABLE staff (staff_id int PRIMARY KEY, name text, role text);
CREATE TABLE orders (
  order_id bigint PRIMARY KEY, room_id int REFERENCES rooms, staff_id int REFERENCES staff,
  order_at timestamp, party_size int, member_phone text, amount numeric);
CREATE TABLE order_items (
  item_id bigint PRIMARY KEY, order_id bigint REFERENCES orders, tea_id int REFERENCES teas, qty int, amount numeric);

SELECT setseed(0.27);
INSERT INTO rooms
SELECT g, (ARRAY['听雨','观山','幽兰','竹韵','大厅A','大厅B','梅坞','半山','云隐','松风','清音','雅集'])[g],
  (ARRAY['包间','包间','茶室','包间','大厅','大厅','茶室','包间','茶室','包间','茶室','包间'])[g],
  (ARRAY[6,8,4,6,20,20,4,8,4,6,4,10])[g]
FROM generate_series(1,12) g;
INSERT INTO teas
SELECT g, (ARRAY['龙井','碧螺春','正山小种','金骏眉','大红袍','铁观音','普洱生','普洱熟','茉莉','白牡丹'])[1+(g%10)]||'-'||g,
  (ARRAY['绿茶','绿茶','红茶','红茶','乌龙','乌龙','普洱','普洱','花茶','白茶'])[1+(g%10)],
  round(((ARRAY[88,128,168,268,388])[1+(g%5)])::numeric,0)
FROM generate_series(1,30) g;
INSERT INTO staff
SELECT g, '员工'||g, (ARRAY['茶艺师','茶艺师','服务员'])[1+(g%3)] FROM generate_series(1,15) g;

INSERT INTO orders (order_id, room_id, staff_id, order_at, party_size, member_phone, amount)
SELECT o.g, o.room_id, o.staff_id, o.order_at, o.party_size,
  CASE WHEN random()<0.35 THEN '1'||(3+floor(random()*7))::int||lpad(floor(random()*1000000000)::text,9,'0') ELSE NULL END,
  0  -- filled from items below
FROM (
  SELECT g, 1+floor(random()*12)::int AS room_id, 1+floor(random()*15)::int AS staff_id,
    date '2025-07-01' + (random()*364)*interval '1 day' + (floor(random()*10)+10)*interval '1 hour' AS order_at,
    (1+floor(random()*8))::int AS party_size
  FROM generate_series(1,20000) g
) o;

INSERT INTO order_items (item_id, order_id, tea_id, qty, amount)
SELECT row_number() over (), x.order_id, x.tea_id, x.qty, round((x.qty*t.price)::numeric,2)
FROM (
  SELECT o.order_id, 1+floor(random()*30)::int AS tea_id, 1+floor(random()*4)::int AS qty
  FROM orders o CROSS JOIN LATERAL generate_series(1, 1+floor(random()*4)::int) i
) x JOIN teas t ON t.tea_id = x.tea_id;

UPDATE orders o SET amount = coalesce((SELECT sum(amount) FROM order_items i WHERE i.order_id=o.order_id),0);
