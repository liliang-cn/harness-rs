-- 华联优选连锁(虚构):连锁零售集团的完整经营账,门店级可一路算到净利。
-- 事实分处不同 grain(销售/进货/损耗/固定成本 按 store×[category]×月;员工按人头快照),
-- 所以 net_profit / 人效 是跨 grain 的 chasm 指标 —— 只有 DI 的 per-grain CTE 算得对。
-- 数据讲一个可诊断的故事:核心商圈的大卖场因高租金 + 高生鲜损耗 + 低人效而亏损。
DROP TABLE IF EXISTS sales, procurement, shrinkage, overheads, employees, categories, stores CASCADE;

CREATE TABLE stores (
  store_id int PRIMARY KEY, name text, region text, city text,
  format text, area_sqm int, open_date date);
CREATE TABLE categories (category_id int PRIMARY KEY, name text);
CREATE TABLE employees (
  emp_id int PRIMARY KEY, store_id int REFERENCES stores, position text,
  monthly_salary numeric, hire_date date, active boolean);
-- 销售(store×category×月):营收、笔数(→客单价)、件数、销售成本(COGS,便于毛利)
CREATE TABLE sales (
  sale_id bigint PRIMARY KEY, store_id int REFERENCES stores, category_id int REFERENCES categories,
  month date, revenue numeric, transactions int, units int, cogs_amount numeric);
-- 进货(store×category×月)
CREATE TABLE procurement (
  proc_id bigint PRIMARY KEY, store_id int REFERENCES stores, category_id int REFERENCES categories,
  month date, purchase_qty int, purchase_amount numeric);
-- 损耗(store×category×月)
CREATE TABLE shrinkage (
  loss_id bigint PRIMARY KEY, store_id int REFERENCES stores, category_id int REFERENCES categories,
  month date, loss_qty int, loss_amount numeric);
-- 门店固定成本(store×月):租金、水电、税
CREATE TABLE overheads (
  oh_id bigint PRIMARY KEY, store_id int REFERENCES stores, month date,
  rent numeric, utilities numeric, tax numeric, labor numeric);

SELECT setseed(0.41);

INSERT INTO stores VALUES
  (1,'华联优选·上海徐汇店','华东','上海','大卖场',8000,'2019-03-01'),
  (2,'华联优选·上海浦东店','华东','上海','标超',2500,'2020-06-01'),
  (3,'华联优选·杭州西湖店','华东','杭州','大卖场',7000,'2018-09-01'),
  (4,'华联优选·南京新街口店','华东','南京','标超',3000,'2021-01-01'),
  (5,'华联优选·广州天河店','华南','广州','大卖场',7500,'2019-11-01'),
  (6,'华联优选·深圳南山店','华南','深圳','标超',2800,'2020-03-01'),
  (7,'华联优选·北京朝阳店','华北','北京','大卖场',8500,'2018-05-01'),
  (8,'华联优选·北京海淀便利店','华北','北京','便利店',120,'2022-04-01'),
  (9,'华联优选·成都锦江店','西部','成都','标超',2600,'2021-07-01'),
  (10,'华联优选·西安钟楼便利店','西部','西安','便利店',150,'2022-08-01');

INSERT INTO categories VALUES
  (1,'生鲜'),(2,'食品'),(3,'日用'),(4,'家电'),(5,'服饰'),(6,'母婴');

-- 员工:每店按业态定编(便利店~8、标超~28、大卖场~65);薪资按城市系数 × 岗位。
INSERT INTO employees (emp_id, store_id, position, monthly_salary, hire_date, active)
SELECT g,
  s.store_id,
  (ARRAY['店长','副店长','收银员','理货员','生鲜员','促销员'])[1+(g%6)],
  round((
     (ARRAY[16000,11000,6000,5500,6500,6000])[1+(g%6)]           -- 岗位基薪
     * (CASE s.city WHEN '上海' THEN 1.35 WHEN '北京' THEN 1.35 WHEN '深圳' THEN 1.30
                    WHEN '杭州' THEN 1.15 WHEN '广州' THEN 1.15 WHEN '南京' THEN 1.05
                    WHEN '成都' THEN 0.95 ELSE 0.90 END)
     * (0.95+random()*0.15))::numeric, 0),
  (date '2019-01-01' + (floor(random()*1800))::int * interval '1 day')::date,
  random() < 0.96
FROM generate_series(1,1) g0
CROSS JOIN LATERAL (
  SELECT st.store_id, st.city, st.format,
         CASE st.format WHEN '便利店' THEN 8 WHEN '标超' THEN 28 ELSE 65 END AS emp_n
  FROM stores st
) s
CROSS JOIN LATERAL generate_series(1, s.emp_n) AS e(idx)
CROSS JOIN LATERAL (SELECT (s.store_id*1000 + e.idx) AS g) gg;

-- 月份轴:2024-01 .. 2025-12(24 个月)
CREATE TEMP TABLE months AS
SELECT (date '2024-01-01' + (m-1) * interval '1 month')::date AS month, m
FROM generate_series(1,24) m;

-- 销售(store×category×月)。品类营收占比 + 毛利率有别(生鲜高营收低毛利);
-- 大卖场核心店营收大;2025 年消费下行,营收较 2024 温和回落;客单价按业态。
INSERT INTO sales (sale_id, store_id, category_id, month, revenue, transactions, units, cogs_amount)
SELECT
  s.store_id*100000 + c.category_id*1000 + mo.m AS sale_id,
  s.store_id, c.category_id, mo.month,
  rev.revenue,
  greatest(1, round(rev.revenue / rev.aov)::int) AS transactions,
  greatest(1, round(rev.revenue / (rev.aov/ (ARRAY[6,4,3,1,2,3])[c.category_id]) )::int) AS units,
  round((rev.revenue * (1 - (ARRAY[0.16,0.24,0.30,0.18,0.38,0.32])[c.category_id]))::numeric, 2) AS cogs_amount
FROM stores s
CROSS JOIN categories c
CROSS JOIN months mo
CROSS JOIN LATERAL (
  SELECT
    -- 门店规模基数 × 品类占比 × 季节 × 年份趋势 × 抖动
    round((
      (CASE s.format WHEN '大卖场' THEN 12000000 WHEN '标超' THEN 4000000 ELSE 800000 END)
      * (ARRAY[0.34,0.26,0.16,0.10,0.08,0.06])[c.category_id]               -- 品类占比:生鲜最大
      * (0.9 + 0.2*sin((mo.m%12)/12.0*2*3.14159))                            -- 季节
      * (CASE WHEN mo.month >= date '2025-01-01' THEN 0.94 ELSE 1.0 END)     -- 2025 消费下行
      * (CASE s.store_id WHEN 7 THEN 0.82 WHEN 5 THEN 0.92 ELSE 1.0 END)     -- 门店健康度(7 号客流下滑)
      * (0.94 + random()*0.12)
    )::numeric, 2) AS revenue,
    (CASE s.format WHEN '便利店' THEN 22 WHEN '标超' THEN 55 ELSE 78 END)     -- 客单价(元)
      * (0.9+random()*0.2) AS aov
) rev;

-- 进货(store×category×月):进货额略高于销售成本(建库存);数量按均价折算。
INSERT INTO procurement (proc_id, store_id, category_id, month, purchase_qty, purchase_amount)
SELECT sale_id, store_id, category_id, month,
  greatest(1, round(cogs_amount / (8 + (category_id*4)))::int),
  round((cogs_amount * (1.02 + random()*0.10))::numeric, 2)
FROM sales;

-- 损耗(store×category×月):损耗率按品类(生鲜 8-14%,其余 1-3%);金额基于进货成本。
INSERT INTO shrinkage (loss_id, store_id, category_id, month, loss_qty, loss_amount)
SELECT s.sale_id, s.store_id, s.category_id, s.month,
  greatest(0, round(p.purchase_qty * lr.rate)::int),
  round((s.cogs_amount * lr.rate)::numeric, 2)
FROM sales s JOIN procurement p ON p.proc_id = s.sale_id
CROSS JOIN LATERAL (
  SELECT (CASE s.category_id
            WHEN 1 THEN 0.08 + random()*0.06     -- 生鲜 8-14%
            WHEN 2 THEN 0.02 + random()*0.02
            ELSE 0.008 + random()*0.02 END)
         * (CASE WHEN s.store_id=7 AND s.category_id=1 THEN 1.6 ELSE 1.0 END) AS rate  -- 7 号生鲜损耗失控
) lr;

-- 门店固定成本(store×月):租金 = 面积 × 城市月租单价 × 业态系数;水电;税(近似增值税额)。
INSERT INTO overheads (oh_id, store_id, month, rent, utilities, tax, labor)
SELECT s.store_id*1000 + mo.m,
  s.store_id, mo.month,
  round((s.area_sqm *
     (CASE s.format WHEN '大卖场' THEN 40 WHEN '标超' THEN 55 ELSE 220 END)      -- 业态月租单价(锚店低、便利店高)
     * (CASE s.city WHEN '上海' THEN 1.4 WHEN '北京' THEN 1.4 WHEN '深圳' THEN 1.3
                    WHEN '杭州' THEN 1.0 WHEN '广州' THEN 1.0 WHEN '南京' THEN 0.85
                    WHEN '成都' THEN 0.7 ELSE 0.7 END)                          -- 城市系数
     * (0.98+random()*0.04))::numeric, 0) AS rent,
  round((s.area_sqm * (18+random()*8))::numeric, 0) AS utilities,
  round((COALESCE((SELECT sum(revenue) FROM sales x WHERE x.store_id=s.store_id AND x.month=mo.month),0)
         * 0.018)::numeric, 0) AS tax
  ,
  (SELECT COALESCE(sum(monthly_salary),0) FROM employees e WHERE e.store_id=s.store_id AND e.active) AS labor
FROM stores s CROSS JOIN months mo;
