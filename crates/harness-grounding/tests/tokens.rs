//! 从 Go 版 `grounding/tokens_test.go` 一条不落移过来的规格。
//!
//! 每一条都是「中文问题召回不到任何东西」这个故障的一个切面：切词把整句话当成
//! 一个 token、FTS 查询串把中文丢干净、中文同义词打不出分。它们不是形式，是规格。

use harness_grounding::lexical::lexical_scores;
use harness_grounding::{fts_query, tokens_of};
use harness_semantic::Model;

/// 与 Go 测试同形的最小模型：指标的同义词是中文。
fn test_model() -> Model {
    Model::from_yaml(
        r#"
entities:
  - {name: order, table: orders, primary_key: id}
metrics:
  - {name: revenue,    entity: order, agg: sum, expr: amount, synonyms: [营收, 销售额], description: "total sales amount"}
  - {name: units_sold, entity: order, agg: sum, expr: qty,    synonyms: [销量, 台数],   description: "units sold"}
"#,
    )
    .expect("model")
}

#[test]
fn tokens_of_cjk_bigrams() {
    let toks = tokens_of("各门店大区的营收");
    for want in ["门店", "大区", "营收"] {
        assert!(toks.contains(want), "expected CJK bigram {want:?} in {toks:?}");
    }
}

#[test]
fn tokens_of_ascii_unchanged() {
    let toks = tokens_of("Revenue by Region");
    assert!(toks.contains("revenue"), "ascii tokens missing: {toks:?}");
    assert!(toks.contains("region"), "ascii tokens missing: {toks:?}");
    assert!(!toks.contains("by"), "stopword leaked into tokens: {toks:?}");
}

#[test]
fn fts_query_keeps_cjk() {
    // 纯中文问题不能塌成一个空的/退化的查询串。
    let q = fts_query("各门店大区的营收");
    assert!(q.contains("营收"), "fts_query dropped CJK, got {q:?}");
}

#[test]
fn mem_lexical_surfaces_by_chinese_synonym() {
    // 同义词是中文的指标，必须能被中文问题打出分：没有 embedder，也不管任何
    // FTS 引擎自带的分词器怎么切。
    let m = test_model();
    let scores = lexical_scores(&m, "各门店大区的营收");
    assert!(
        scores.get("revenue").copied().unwrap_or(0.0) > 0.0,
        "revenue not surfaced by Chinese synonym; scores={scores:?}"
    );
}
