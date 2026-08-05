//! 把一句中文问题落到语义层的 `{metrics, group_by, grain}` 上 —— 确定性的那一半。
//!
//! [`harness_semantic`] 解决的是「一个指标按几个维度怎么编译成不会算错的 SQL」。
//! 这个 crate 解决它前面那一步：用户说的是「各茶类的营收」，得先变成
//! `{metrics: [tea_revenue], group_by: [tea_category]}`。
//!
//! # 为什么这需要一个 crate
//!
//! **中文不写空格。** 一个按空白切词的匹配器读「各门店大区的营收」只会得到一个
//! token（整句话），于是模型里写好的同义词「营收」永远匹配不上——词法召回在中文
//! 问题上不是精度下降，是直接归零。所以这里自带一套 CJK 感知的切分（ASCII 走词，
//! CJK 走重叠二元组），见 [`tokens`]。
//!
//! **猜错没有人会发现。** 检索按相似度排序，而一个模型里的指标本来就都在讲同一门
//! 生意：「总电耗」（一个求和）召回成「吨件电耗」（一个比率），排第一，后面跟着一个
//! 真算出来的数字。一个跑得干干净净的错数字，正是这一层存在的理由。所以召回可以
//! 模糊，**承诺只认逐字证据**；证据并列时返回 [`Clarify`]，见 [`ground`]。
//!
//! **切得动才算数。** 「营收」逐字命中 order 粒度的 revenue，但 order 到 tea 没有
//! 安全连接路径——所以词法命中之后还要过一遍 `Model::dimensions_for`，切不动就换
//! 一个切得动的（tea_revenue），换不出来就把冲突说出来。
//!
//! # 现在没有的那一半
//!
//! LLM 生成与向量召回不在这里。[`recall::Recall`] 是留给它们的接口位；没有实现时
//! 整套东西退化成**纯词法召回**——这是可用的降级，不是缺陷：只要模型把中文同义词
//! 写全（`synonyms: [营收, 营业额, 流水]`），词法这一半就能独立落地，而且它是可测的、
//! 可解释的、离线的。向量补的是长尾（同义词没写全时「进账」也能找到 revenue），
//! LLM 补的是组合式的复杂问句。两者都只该影响**候选**，不该影响**承诺**。
//!
//! ```
//! use harness_grounding::Grounder;
//! use harness_semantic::Model;
//!
//! let m = Model::from_yaml(r#"
//! entities:
//!   - {name: order, table: orders, primary_key: id}
//!   - {name: room,  table: rooms,  primary_key: room_id}
//! joins:
//!   - {from: order, to: room, from_key: room_id, to_key: room_id, cardinality: many_to_one}
//! dimensions:
//!   - {name: room_type, entity: room, column: room_type, type: categorical, synonyms: [包间类型]}
//! metrics:
//!   - {name: revenue, entity: order, agg: sum, expr: amount, synonyms: [营收]}
//! "#).unwrap();
//!
//! let g = Grounder::new(&m).ground("按包间类型看营收");
//! assert_eq!(g.metrics, ["revenue"]);
//! assert_eq!(g.group_by, ["room_type"]);
//! ```

pub mod ground;
pub mod lexical;
pub mod recall;
pub mod tokens;

pub use ground::{Clarify, Grounded, Grounder};
pub use lexical::{ScoredMetric, lexical_scores, retrieve};
pub use recall::Recall;
pub use tokens::{fts_query, tokens_of};
