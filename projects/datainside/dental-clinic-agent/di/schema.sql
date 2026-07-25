-- 口腔连锁诊所: appointment grain vs treatment grain vs chair-capacity grain.
-- -> chair_utilization 跨 treatment/capacity grain, DI 各自 CTE 聚合 chasm-safe.
--    revenue/gross_margin/margin_rate 走 finance 授权; 患者手机号脱敏.
DROP TABLE IF EXISTS treatments, appointments, chair_capacity, patients, dentists, clinics CASCADE;
CREATE TABLE clinics (clinic_id int PRIMARY KEY, name text, city text);
CREATE TABLE dentists (dentist_id int PRIMARY KEY, name text, clinic_id int REFERENCES clinics, title text);
CREATE TABLE patients (patient_id int PRIMARY KEY, name text, phone text, gender text, tier text);
CREATE TABLE appointments (
  appt_id bigint PRIMARY KEY, clinic_id int REFERENCES clinics, dentist_id int REFERENCES dentists,
  patient_id int REFERENCES patients, scheduled_at timestamp, status text, booked_min int);
CREATE TABLE treatments (
  treatment_id bigint PRIMARY KEY, appt_id bigint REFERENCES appointments, item_name text,
  category text, amount numeric, cost_amount numeric, actual_min int);
CREATE TABLE chair_capacity (
  cap_id bigint PRIMARY KEY, clinic_id int REFERENCES clinics, day date, available_min int);

SELECT setseed(0.41);
INSERT INTO clinics VALUES
  (1,'旗舰口腔门诊部','上海'),(2,'徐汇口腔诊所','上海'),(3,'浦东口腔诊所','上海'),
  (4,'天河口腔门诊部','广州'),(5,'越秀口腔诊所','广州'),(6,'福田口腔门诊部','深圳'),
  (7,'南山口腔诊所','深圳'),(8,'西湖口腔诊所','杭州');
INSERT INTO dentists
SELECT g, '医生-'||g, 1+(g%8), (ARRAY['主任医师','副主任医师','主治医师','住院医师'])[1+(g%4)]
FROM generate_series(1,40) g;
INSERT INTO patients
SELECT g, '患者-'||g,
  CASE WHEN random()<0.3 THEN '1'||(3+floor(random()*7))::int||lpad(floor(random()*1000000000)::text,9,'0') END,
  (ARRAY['男','女'])[1+(g%2)],
  (ARRAY['普通','普通','银卡','金卡'])[1+(g%4)]
FROM generate_series(1,15000) g;

-- 椅位可售分钟: 每门诊每天 = 椅位数(2..5) × 600 分钟营业时长.
INSERT INTO chair_capacity (cap_id, clinic_id, day, available_min)
SELECT row_number() OVER (), c.clinic_id, d::date, (2+(c.clinic_id%4))*600
FROM clinics c CROSS JOIN generate_series(date '2025-07-01', date '2026-06-29', interval '1 day') d;

-- 预约 (appointment grain): clinic_id 随医生所属门诊, 状态 到诊/爽约/取消 ≈ 70/15/15.
INSERT INTO appointments (appt_id, clinic_id, dentist_id, patient_id, scheduled_at, status, booked_min)
SELECT s.g, d.clinic_id, s.dentist_id, s.patient_id, s.scheduled_at, s.status, s.booked_min
FROM (
  SELECT g,
    1+floor(random()*40)::int AS dentist_id,
    1+floor(random()*15000)::int AS patient_id,
    date '2025-07-01' + (random()*363)*interval '1 day' + (8+random()*10)*interval '1 hour' AS scheduled_at,
    (ARRAY['attended','attended','attended','attended','attended','attended','attended','attended',
           'attended','attended','attended','attended','attended','attended',
           'no_show','no_show','no_show','cancelled','cancelled','cancelled'])[1+floor(random()*20)::int] AS status,
    (ARRAY[30,30,45,45,60,90])[1+floor(random()*6)::int] AS booked_min
  FROM generate_series(1,45000) g
) s JOIN dentists d ON d.dentist_id = s.dentist_id;

-- 诊疗项目 (treatment grain): 每个到诊预约 1..2 项; category 决定金额/成本/实际椅位分钟.
-- cost_amount 反范式写在诊疗行上, gross_margin 无需 join.
INSERT INTO treatments (treatment_id, appt_id, item_name, category, amount, cost_amount, actual_min)
SELECT row_number() OVER (), p.appt_id, p.category||'诊疗', p.category,
  p.amount,
  round((p.amount*(0.35+random()*0.20))::numeric,2),
  p.act_min
FROM (
  SELECT k.appt_id, k.category,
    CASE k.category
      WHEN '种植' THEN round((8000+random()*7000)::numeric,2)
      WHEN '正畸' THEN round((3000+random()*5000)::numeric,2)
      WHEN '补牙' THEN round(( 200+random()* 500)::numeric,2)
      WHEN '拔牙' THEN round(( 150+random()* 500)::numeric,2)
      ELSE            round(( 120+random()* 180)::numeric,2)  -- 洁牙
    END AS amount,
    CASE k.category
      WHEN '种植' THEN  90+floor(random()*40)::int
      WHEN '正畸' THEN  25+floor(random()*20)::int
      WHEN '补牙' THEN  35+floor(random()*30)::int
      WHEN '拔牙' THEN  25+floor(random()*25)::int
      ELSE             30+floor(random()*20)::int  -- 洁牙
    END AS act_min
  FROM (
    SELECT a.appt_id,
      (ARRAY['洁牙','洁牙','洁牙','洁牙','洁牙','洁牙','洁牙','补牙','补牙','补牙','补牙','补牙','补牙',
             '拔牙','拔牙','拔牙','种植','种植','正畸','正畸'])[1+floor(random()*20)::int] AS category
    FROM appointments a
    CROSS JOIN generate_series(1, 1 + ((a.appt_id % 10) < 9)::int) g
    WHERE a.status='attended'
  ) k
) p;
