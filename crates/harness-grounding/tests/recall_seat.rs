//! 语义召回的接口位：本 crate 不带实现，但融合逻辑现在就得是对的，
//! 否则等哪天接上 embedding，出问题的会是这一层而不是那个模型。

use harness_grounding::{Grounder, Recall};

/// 一个替身：只认「文档里出现了某个字符串」，用来把召回信号推到极端。
struct Stub {
    prefer: &'static str,
    /// 故意返回错长度，用来验证「实现方没守约定」时的行为。
    truncate: bool,
}

impl Recall for Stub {
    fn similarity(&self, _question: &str, docs: &[String]) -> Vec<f64> {
        let mut out: Vec<f64> = docs
            .iter()
            .map(|d| if d.contains(self.prefer) { 1.0 } else { 0.0 })
            .collect();
        if self.truncate {
            out.truncate(1);
        }
        out
    }
}

fn model() -> harness_semantic::Model {
    harness_semantic::Model::from_yaml(include_str!("models/teahouse.yaml")).expect("model")
}

/// 语义召回可以把一个一个字都对不上的指标拉进候选名单——这正是它的用处
/// （同义词没写全的长尾）。但它**不能**改变最终落到哪个指标上：
/// 承诺只认逐字证据，一个余弦分数没法向任何人解释它为什么更对。
#[test]
fn recall_widens_candidates_but_never_decides() {
    let m = model();
    let stub = Stub {
        prefer: "orders_count",
        truncate: false,
    };
    let g = Grounder::new(&m)
        .with_recall(Box::new(stub))
        .ground("各茶类的营收");

    let names: Vec<&str> = g.candidates.iter().map(|c| c.name.as_str()).collect();
    assert!(
        names.contains(&"orders_count"),
        "语义召回没能把无词法证据的指标带进候选: {names:?}"
    );
    assert_eq!(g.metrics, ["tea_revenue"], "召回信号不该改变判定: {g:?}");
}

/// 逐字命中是分数下界。语义模型有权认为「营收」跟什么都一样像（这里给全 0），
/// 但没有权把一个确实出现在问题里的同义词压出候选名单。
#[test]
fn verbatim_lexical_hit_floors_the_fused_score() {
    let m = model();
    let stub = Stub {
        prefer: "\u{0}", // 谁都不像 → dense 全 0 → 归一化后全 0
        truncate: false,
    };
    let g = Grounder::new(&m)
        .with_recall(Box::new(stub))
        .ground("各茶类的营收");

    let names: Vec<&str> = g.candidates.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"revenue"), "词法下界失效: {names:?}");
    assert!(names.contains(&"tea_revenue"), "{names:?}");
}

/// 实现方返回的长度对不上时，忽略这个信号退回纯词法——错位打分比没有打分更坏。
#[test]
fn malformed_recall_degrades_to_lexical() {
    let m = model();
    let bad = Stub {
        prefer: "orders_count",
        truncate: true,
    };
    let with_bad = Grounder::new(&m)
        .with_recall(Box::new(bad))
        .ground("各茶类的营收");
    let lexical_only = Grounder::new(&m).ground("各茶类的营收");

    assert_eq!(with_bad.candidates, lexical_only.candidates);
    assert_eq!(with_bad.metrics, lexical_only.metrics);
}

/// 模式写进回执：一份答案是怎么来的，事后必须说得清。
#[test]
fn mode_is_reported() {
    let m = model();
    assert_eq!(Grounder::new(&m).mode(), "lexical(cjk)+rerank");
    let stub = Stub {
        prefer: "x",
        truncate: false,
    };
    assert_eq!(
        Grounder::new(&m).with_recall(Box::new(stub)).mode(),
        "lexical(cjk)+recall+rerank"
    );
}
