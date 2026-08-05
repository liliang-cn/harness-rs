# harness-rs-semantic

一个受治理的语义层:模型(实体、join、维度、指标)+ 一个把「**这个指标按这些维度**」
编译成 fan-out / chasm 安全 SQL 的编译器。

不说 LLM,不开数据库连接。它只产出 SQL 字符串。

## 为什么要有它

让模型自己写 SQL——哪怕只读——**挡不住它编一个错的 join、挑错 grain、把总数经
一对多 join 扇出成一个自信、干净、错误的数字**。这些失败不是差一个 `WHERE`,
是结构性的,再多提示词也消不掉。

所以 agent 不写 SQL。它要一个**指标 × 维度**,由这里编译。核心手法一句话:

> **先把每个度量在它自己的 CTE 里聚合到基础粒度,然后才 join 维度。**

fan-out 和 chasm join 因此在构造上不可能发生,而不是靠 review 时有人看出来。

## 用

```rust
use harness_semantic::{Model, Query, dialect};

let m = Model::from_yaml(yaml)?;
let q = Query { metrics: vec!["revenue".into()], group_by: vec!["region".into()], ..Default::default() };
let out = harness_semantic::compile(&m, &q, &dialect::Postgres)?;
// out.sql / out.args
```

方言:postgres · mysql · sqlite · sqlserver · snowflake · databricks · duckdb · ansi

## 出处

从 `semantic-go` 移植。那边 534 行测试**每一条都是一个用昂贵方式发现的失败**——
一条跑得干干净净、返回了错误数字的查询——所以它们被一并移过来当规格,而不是当
形式。移植后与 Go 版做过差分:13 个生产模型 × 5 种方言 × 2 种分组,
**130/130 逐字节一致**,包括 chasm 拒绝时的那句错误信息。
