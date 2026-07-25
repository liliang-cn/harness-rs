-- Hotel & B&B chain: stay grain vs capacity grain. occupancy/revpar cross both
-- -> only DI's per-grain CTEs stay chasm-safe. revenue/revpar/adr finance-gated.
DROP TABLE IF EXISTS stays, room_capacity, rooms, guests, hotels CASCADE;
CREATE TABLE hotels (hotel_id int PRIMARY KEY, name text, city text);
CREATE TABLE rooms (room_id int PRIMARY KEY, hotel_id int REFERENCES hotels, room_type text, base_price numeric);
CREATE TABLE guests (guest_id int PRIMARY KEY, name text, phone text, tier text);
CREATE TABLE room_capacity (cap_id int PRIMARY KEY, hotel_id int REFERENCES hotels, day date, available_room_nights int);
CREATE TABLE stays (
  stay_id int PRIMARY KEY, hotel_id int REFERENCES hotels, room_id int REFERENCES rooms,
  guest_id int REFERENCES guests, checkin date, nights int, room_nights int,
  amount numeric, status text);

SELECT setseed(0.42);
INSERT INTO hotels VALUES
  (1,'云栖·北京王府井店','北京'),(2,'云栖·上海外滩店','上海'),(3,'云栖·广州珠江店','广州'),
  (4,'云栖·深圳南山店','深圳'),(5,'云栖·成都宽窄巷店','成都'),(6,'云栖·杭州西湖店','杭州'),
  (7,'云栖·西安钟楼店','西安'),(8,'云栖·重庆洪崖洞店','重庆'),(9,'云栖·南京夫子庙店','南京'),
  (10,'云栖·苏州平江店','苏州'),(11,'云栖·厦门鼓浪屿店','厦门'),(12,'云栖·丽江古城店','丽江');

-- 600 rooms, 50 per hotel (hotel_id = 1+g%12); base_price by room_type × hotel factor.
INSERT INTO rooms (room_id, hotel_id, room_type, base_price)
SELECT g, 1+(g%12),
  (ARRAY['标准间','大床房','套房','家庭房'])[1+(g%4)],
  round(((ARRAY[280,420,880,560])[1+(g%4)] * (0.85 + (g%12)*0.03) * (0.9+random()*0.2))::numeric, 2)
FROM generate_series(1,600) g;

-- 30k guests; valid CN mobile in ~30% else NULL (PII).
INSERT INTO guests
SELECT g, '客人-'||g,
  CASE WHEN random()<0.3
    THEN '1'||(3+floor(random()*7))::int||lpad(floor(random()*1000000000)::text,9,'0')
    ELSE NULL END,
  (ARRAY['普通','普通','银卡','金卡','钻石'])[1+floor(random()*5)::int]
FROM generate_series(1,30000) g;

-- Capacity grain: every hotel × every day of 2025, sellable = that hotel's room count.
INSERT INTO room_capacity (cap_id, hotel_id, day, available_room_nights)
SELECT row_number() OVER (), h.hotel_id, d::date,
  (SELECT count(*)::int FROM rooms r WHERE r.hotel_id = h.hotel_id)
FROM hotels h
CROSS JOIN generate_series(timestamp '2025-01-01', timestamp '2025-12-31', interval '1 day') d;

-- Stay grain: 60k stays. Volatile random() per row in a derived table over
-- generate_series (never LATERAL, which would materialize once). status 85/8/7
-- via one cumulative uniform; nights skewed to 1-2; room_nights = nights.
INSERT INTO stays (stay_id, hotel_id, room_id, guest_id, checkin, nights, room_nights, amount, status)
SELECT s.g, r.hotel_id, s.room_id, s.guest_id, s.checkin, s.nights, s.nights,
  round((s.nights * r.base_price * (0.85+random()*0.7))::numeric, 2),
  CASE WHEN s.u < 0.85 THEN 'checked_out' WHEN s.u < 0.93 THEN 'no_show' ELSE 'cancelled' END
FROM (
  SELECT g, 1+floor(random()*600)::int AS room_id, 1+floor(random()*30000)::int AS guest_id,
    (date '2025-01-01' + (floor(random()*365))::int * interval '1 day')::date AS checkin,
    (ARRAY[1,1,1,2,2,2,3,3,4,7])[1+floor(random()*10)::int] AS nights,
    random() AS u
  FROM generate_series(1,60000) g
) s JOIN rooms r ON r.room_id = s.room_id;
