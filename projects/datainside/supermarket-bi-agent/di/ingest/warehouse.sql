-- 治理仓的目标结构(强类型、主键、外键)——落库的「契约」。
-- 源数据无论多脏,最终都要落进这张固定结构里;DI 的治理指标默认它是干净的。
-- 空表:数据由落库流程(load.sh)从 staging 校验后灌入。
DROP TABLE IF EXISTS sales, inventory, members, products, departments CASCADE;
DROP TABLE IF EXISTS stg_departments, stg_products, stg_members, stg_inventory, stg_sales CASCADE;

CREATE TABLE departments (dept_id int PRIMARY KEY, name text);
CREATE TABLE products (
  sku int PRIMARY KEY, name text, dept_id int REFERENCES departments,
  category text, price numeric, cost numeric);
CREATE TABLE members (member_id int PRIMARY KEY, phone text, tier text);
CREATE TABLE inventory (sku int PRIMARY KEY REFERENCES products, qty int, days_in_stock int);
CREATE TABLE sales (
  sale_id bigint PRIMARY KEY, sku int REFERENCES products, member_id int REFERENCES members,
  sold_at timestamp, qty int, amount numeric, cost_amount numeric);
