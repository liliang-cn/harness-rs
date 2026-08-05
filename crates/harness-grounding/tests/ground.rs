//! 端到端：两个真实模型（茶馆、锻造厂），真实的中文问句。
//!
//! 每一条都跑到最后一步——把落地结果交给 `harness_semantic::compile`。落地对不对，
//! 最终的判据不是「字符串等于我期望的名字」，而是「编译出来的 SQL 敢不敢给老板看」。

use harness_grounding::Grounder;
use harness_semantic::{Model, Query, compile, dialect};

fn teahouse() -> Model {
    Model::from_yaml(include_str!("models/teahouse.yaml")).expect("teahouse model")
}

fn forge() -> Model {
    Model::from_yaml(include_str!("models/forge.yaml")).expect("forge model")
}

fn sql_of(m: &Model, g: &harness_grounding::Grounded) -> String {
    compile(m, &g.to_query(), &dialect::Postgres)
        .unwrap_or_else(|e| panic!("落地结果编译失败: {e}\n{g:?}"))
        .sql
}

/// 「各茶类的营收」——本 crate 存在的理由。
///
/// 「营收」逐字命中的是 order 粒度的 revenue，但 order 到 tea 没有安全连接路径：
/// revenue by tea_category 要么编译失败，要么（在一个更宽松的编译器里）把总额沿
/// 着 tea 连接放大。真正被问的是 item 粒度的 tea_revenue。
#[test]
fn teahouse_tea_revenue_by_tea_category() {
    let m = teahouse();
    let g = Grounder::new(&m).ground("各茶类的营收");

    assert_eq!(g.metrics, ["tea_revenue"], "{g:?}");
    assert_eq!(g.group_by, ["tea_category"], "{g:?}");
    assert!(g.clarify.is_none(), "{g:?}");
    assert!(sql_of(&m, &g).contains("SUM(amount)"));

    // 被换掉的那个读法确实是死路：这就是不能直接信「营收」的原因。
    let naive = Query {
        metrics: vec!["revenue".into()],
        group_by: vec!["tea_category".into()],
        ..Default::default()
    };
    assert!(
        compile(&m, &naive, &dialect::Postgres).is_err(),
        "revenue 竟然能按 tea_category 编译——那这一层的判断就白做了"
    );
}

/// 同一个「营收」，换一个切法就该老老实实落到 revenue 上：room 是 order 能安全
/// 连到的，没有任何理由改写用户点的名。
#[test]
fn teahouse_revenue_by_room_type() {
    let m = teahouse();
    let g = Grounder::new(&m).ground("按包间类型看营收");

    assert_eq!(g.metrics, ["revenue"], "{g:?}");
    assert_eq!(g.group_by, ["room_type"], "{g:?}");
    assert!(sql_of(&m, &g).contains("room_type"));
}

/// 「各产线的产量」。模型里没有任何指标的同义词写着「产量」——只有窗口指标
/// output_ytd 的同义词「年累计产量」蹭上了一个二元组。证据是部分的，而且问题里
/// 没有任何时间轴，所以要的是它底下那个基础量 output_units。
#[test]
fn forge_output_units_by_line() {
    let m = forge();
    let g = Grounder::new(&m).ground("各产线的产量");

    assert_eq!(g.metrics, ["output_units"], "{g:?}");
    assert_eq!(g.group_by, ["line_name"], "{g:?}");
    assert!(g.grain.is_empty(), "{g:?}");
    assert!(sql_of(&m, &g).contains("SUM(output_units)"));
}

/// 反过来：用户逐字点了「年累计产量」，就不许降级成基础量。窗口指标需要时间轴，
/// 而 run 只能安全连到 run_time 一个时间维度——补上它，不用问。
#[test]
fn forge_window_metric_keeps_its_time_axis() {
    let m = forge();
    let g = Grounder::new(&m).ground("各产线的年累计产量");

    assert_eq!(g.metrics, ["output_ytd"], "{g:?}");
    assert_eq!(g.group_by, ["line_name", "run_time"], "{g:?}");
    let sql = sql_of(&m, &g);
    assert!(sql.contains("PARTITION BY"), "{sql}");
}

/// 时间粒度：「每月」是粒度，不是维度。识别出粒度就得补一根时间轴，
/// 否则编译器会拒绝（GrainMatchedNothing）——这正是它该拒绝的。
#[test]
fn teahouse_month_grain_picks_the_time_axis() {
    let m = teahouse();
    let g = Grounder::new(&m).ground("每月的营收");

    assert_eq!(g.metrics, ["revenue"], "{g:?}");
    assert_eq!(g.grain, "month", "{g:?}");
    assert_eq!(g.group_by, ["order_time"], "{g:?}");
    assert!(sql_of(&m, &g).contains("date_trunc('month'"));
}

/// 锻造厂有两个时间维度（run_time 和 order_time）。选哪根轴不靠猜，
/// 靠连接图：sales_revenue 从 sales_order 出发，根本连不到 run_time。
#[test]
fn forge_month_grain_disambiguated_by_reachability() {
    let m = forge();
    let g = Grounder::new(&m).ground("按月看销售额");

    assert_eq!(g.metrics, ["sales_revenue"], "{g:?}");
    assert_eq!(g.group_by, ["order_time"], "{g:?}");
    assert_eq!(g.grain, "month", "{g:?}");
    sql_of(&m, &g);
}

/// 一句话点了两个指标就返回两个指标（顺序按模型定义序，不按问题里谁先出现）。
#[test]
fn teahouse_two_metrics_in_one_question() {
    let m = teahouse();
    let g = Grounder::new(&m).ground("营收和客单价");

    assert_eq!(g.metrics, ["revenue", "avg_ticket"], "{g:?}");
    assert!(g.group_by.is_empty(), "{g:?}");
    sql_of(&m, &g);
}

/// 更长的说法先占位：「净营收」占住了自己那一段，里面的「营收」就不能再被另一个
/// 指标认领一次。没有这条规则，一句话会同时落到两个互相竞争的指标上。
#[test]
fn longer_phrase_claims_its_span() {
    let m = competing_model();
    let g = Grounder::new(&m).ground("净营收是多少");

    assert_eq!(g.metrics, ["net_revenue"], "{g:?}");
}

/// 两个指标声明了同一个说法，且同样权威（都是同义词）。模型没能替用户区分开的
/// 东西，落地器也不替他区分：反问，而不是按声明顺序挑一个。
#[test]
fn tied_synonyms_ask_instead_of_guessing() {
    let m = competing_model();
    let g = Grounder::new(&m).ground("营收是多少");

    assert!(g.metrics.is_empty(), "{g:?}");
    let c = g.clarify.expect("应该反问");
    let mut names = c.candidates.clone();
    names.sort();
    assert_eq!(names, ["gross_revenue", "net_revenue"], "{c:?}");
}

fn competing_model() -> Model {
    Model::from_yaml(
        r#"
entities:
  - {name: order, table: orders, primary_key: id}
metrics:
  - {name: gross_revenue, entity: order, agg: sum, expr: amount,     synonyms: [营收, 毛营收]}
  - {name: net_revenue,   entity: order, agg: sum, expr: amount_net, synonyms: [营收, 净营收]}
"#,
    )
    .expect("competing model")
}

/// 一个词都对不上就不猜。反问时列出的是**这个维度切得动**的指标，
/// 不是整本目录：十个名字是一个问题，四十四个是耸肩。
#[test]
fn unknown_measure_asks_with_sliceable_candidates() {
    let m = teahouse();
    let g = Grounder::new(&m).ground("各茶类的销量");

    assert!(g.metrics.is_empty(), "{g:?}");
    let c = g.clarify.expect("应该反问");
    assert!(c.candidates.contains(&"tea_revenue".to_string()), "{c:?}");
    assert!(c.candidates.contains(&"tea_units".to_string()), "{c:?}");
    assert!(
        !c.candidates.contains(&"revenue".to_string()),
        "revenue 按 tea_category 切不动，不该出现在候选里: {c:?}"
    );
    assert!(c.candidates.len() <= 10, "{c:?}");
}

/// 点名的指标切不动这个维度，又找不到有证据的替代品：把冲突说出来。
/// 人均消费是 order 粒度的比率，怎么绕都到不了 tea。
#[test]
fn cross_grain_request_is_refused_not_approximated() {
    let m = teahouse();
    let g = Grounder::new(&m).ground("各茶类的人均消费");

    assert!(g.metrics.is_empty(), "{g:?}");
    let c = g.clarify.expect("应该反问");
    assert!(c.question.contains("spend_per_guest"), "{c:?}");
    assert!(c.question.contains("tea_category"), "{c:?}");
}

/// 英文问句照走同一条路。`shift` 既是维度 `shift` 的名字，也是 `plan_shift` 的
/// 物理列名——规范名压过列名，和模型自己的同义词索引是同一条规则。
#[test]
fn english_question_and_name_beats_column() {
    let m = forge();
    let g = Grounder::new(&m).ground("output units by shift");

    assert_eq!(g.metrics, ["output_units"], "{g:?}");
    assert_eq!(g.group_by, ["shift"], "{g:?}");
    sql_of(&m, &g);
}

/// 召回名单是给人看的凭据，也是将来喂给 LLM 的短名单：它按分数排序，长度受
/// top_k 约束，但**不影响判定**——判定走的是穷举的词法证据。
#[test]
fn candidates_are_a_ranked_receipt() {
    let m = teahouse();
    let g = Grounder::new(&m).with_top_k(3).ground("各茶类的营收");

    assert!(!g.candidates.is_empty());
    assert!(g.candidates.len() <= 3);
    for w in g.candidates.windows(2) {
        assert!(w[0].score >= w[1].score, "候选没有按分数排序: {:?}", g.candidates);
    }
    assert_eq!(g.metrics, ["tea_revenue"], "top_k 截断不该改变判定: {g:?}");
}
