-- EduMind AI Smart Tutor Database
DROP TABLE IF EXISTS test_results CASCADE;
DROP TABLE IF EXISTS student_errors CASCADE;
DROP TABLE IF EXISTS questions CASCADE;
DROP TABLE IF EXISTS knowledge_nodes CASCADE;
DROP TABLE IF EXISTS students CASCADE;

CREATE TABLE students (
  student_id text PRIMARY KEY,
  name text NOT NULL,
  grade text NOT NULL,
  learning_style text NOT NULL,
  created_at timestamp NOT NULL
);

CREATE TABLE knowledge_nodes (
  node_id text PRIMARY KEY,
  subject text NOT NULL,
  name text NOT NULL,
  category text NOT NULL,
  difficulty_level int NOT NULL,
  prerequisite_node_id text
);

CREATE TABLE questions (
  question_id bigint PRIMARY KEY,
  node_id text REFERENCES knowledge_nodes(node_id),
  title text NOT NULL,
  content text NOT NULL,
  answer text NOT NULL,
  difficulty int NOT NULL
);

CREATE TABLE student_errors (
  error_id bigint PRIMARY KEY,
  student_id text REFERENCES students(student_id),
  node_id text REFERENCES knowledge_nodes(node_id),
  question_id bigint REFERENCES questions(question_id),
  error_category text NOT NULL, -- '概念混淆', '计算失误', '步骤遗漏', '审题偏差'
  user_answer text NOT NULL,
  corrected boolean DEFAULT false,
  created_at timestamp NOT NULL
);

CREATE TABLE test_results (
  test_id bigint PRIMARY KEY,
  student_id text REFERENCES students(student_id),
  subject text NOT NULL,
  score numeric(5,2) NOT NULL,
  total_questions int NOT NULL,
  correct_count int NOT NULL,
  tested_at timestamp NOT NULL
);

SELECT setseed(0.2026);

-- 1. Students
INSERT INTO students VALUES
 ('student_10086', '小明', '高一', '启发引导型', '2025-09-01 08:00:00'),
 ('student_10087', '小华', '高二', '视觉推导型', '2025-09-01 08:00:00');

-- 2. Knowledge Nodes (Math & Physics)
INSERT INTO knowledge_nodes VALUES
 ('MATH_01', '数学', '一元二次方程配方法', '代数', 2, NULL),
 ('MATH_02', '数学', '二次函数顶点式与性质', '函数', 3, 'MATH_01'),
 ('MATH_03', '数学', '三角函数诱导公式', '三角学', 3, NULL),
 ('MATH_04', '数学', '平面向量数量积', '向量', 4, NULL),
 ('MATH_05', '数学', '导数与极值分析', '微积分', 5, 'MATH_02'),
 ('PHYS_01', '物理', '牛顿第二定律', '力学', 3, NULL),
 ('PHYS_02', '物理', '平抛运动与动能定理', '力学', 4, 'PHYS_01');

-- 3. Questions
INSERT INTO questions VALUES
 (101, 'MATH_01', '求方程 x^2 - 6x + 5 = 0 的根（使用配方法）', '使用配方法解方程 x^2 - 6x + 5 = 0', 'x1=5, x2=1', 2),
 (102, 'MATH_02', '求二次函数 y = x^2 - 4x + 7 的顶点坐标与最小值', '已知二次函数 y = x^2 - 4x + 7', '(2, 3), 最小值为 3', 3),
 (103, 'MATH_03', '化简 sin(π - α) + cos(π/2 + α)', '化简三角函数表达式', '0', 3),
 (104, 'PHYS_01', '质量为 2kg 的物体在 10N 水平拉力作用下沿光滑平面加速，求加速度', '求加速度 a', 'a = 5 m/s^2', 2);

-- 4. Student Errors (Historical Weaknesses)
INSERT INTO student_errors VALUES
 (1, 'student_10086', 'MATH_01', 101, '步骤遗漏', '(x-3)^2 = 4 -> x-3 = 2 -> x=5', true, '2025-10-10 14:00:00'),
 (2, 'student_10086', 'MATH_02', 102, '计算失误', 'y = (x-2)^2 + 3 -> 顶点 (2, 7)', false, '2025-10-15 16:30:00'),
 (3, 'student_10086', 'MATH_03', 103, '概念混淆', 'sin(π-α) = -sin(α)', false, '2025-10-18 10:00:00');

-- 5. Test Results
INSERT INTO test_results VALUES
 (1, 'student_10086', '数学', 78.50, 10, 7, '2025-10-10 17:00:00'),
 (2, 'student_10086', '数学', 82.00, 10, 8, '2025-10-17 17:00:00'),
 (3, 'student_10086', '物理', 90.00, 10, 9, '2025-10-19 17:00:00');
