//! 词法召回与重排 —— 把一个问题和模型里的几十个指标放在一起打分排序。
//!
//! 两个阶段刻意分开，是因为它们回答的是不同的问题：
//!
//! * **召回**（[`retrieve`]）：问题与指标文档（名字+同义词+描述）各自独立打分，
//!   便宜、可并行，负责「别把对的那个漏掉」。
//! * **重排**（cross-encoder）：把 (问题, 指标) 当成一对联合打分——这个指标的
//!   名字/同义词到底被问题覆盖了多少。这是 precision@1 的那一步：一个逐字命中的
//!   指标必须排在一个「主题上也挺像」的邻居前面，否则最后落地的就是那个邻居。

use crate::recall::Recall;
use crate::tokens::{find_span, normalize, tokens_of};
use harness_semantic::{Metric, Model};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// 一条召回结果 —— 名字 + 分数，构成给上层看的「凭据」。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScoredMetric {
    pub name: String,
    pub score: f64,
}

/// 指标的可检索文档：名字 + 同义词 + 描述。
///
/// 同义词是承重的：模型里写下 `synonyms: [营收, 营业额, 流水]` 这一行，就是这套
/// 东西能听懂中文的全部原因。描述次之——它能帮上忙（tea_revenue 的描述里出现了
/// 「茶类」），但只靠描述命中不足以承诺一个指标，见 `ground` 里的证据门槛。
pub fn metric_doc(m: &Metric) -> String {
    format!(
        "{} {} {}",
        m.name,
        m.synonyms.join(" "),
        m.description
    )
}

/// 指标可用于逐字匹配的标签：规范名 + 同义词。
pub fn metric_labels(m: &Metric) -> Vec<String> {
    let mut out = Vec::with_capacity(1 + m.synonyms.len());
    out.push(m.name.clone());
    out.extend(m.synonyms.iter().cloned());
    out
}

/// 问题与每个指标文档的 token 重叠度（命中数 / 问题 token 数）。
///
/// 纯内存、纯确定：不依赖 embedding，也不依赖任何 FTS 引擎自带的分词器。
/// 中文问题能不能召回，取决于这里的二元组切法，而不是取决于 sqlite 的心情。
pub fn lexical_scores(model: &Model, question: &str) -> BTreeMap<String, f64> {
    let q = tokens_of(question);
    let mut out = BTreeMap::new();
    if q.is_empty() {
        return out;
    }
    for m in &model.metrics {
        let d = tokens_of(&metric_doc(m));
        let hit = q.iter().filter(|t| d.contains(*t)).count();
        if hit > 0 {
            out.insert(m.name.clone(), hit as f64 / q.len() as f64);
        }
    }
    out
}

/// (问题, 标签集) 的联合覆盖度，也是本 crate 唯一被用来**承诺**的证据：
///
/// * 1.0 —— 某个标签整条逐字出现在问题里（「营收」出现在「按包间类型看营收」中）。
///   模型自己声明的说法出现在问题里，这是命名，不是猜测。
/// * 否则 —— 覆盖率最高的那个标签的 token 命中比例：问题的词覆盖了这个标签的几分之几。
///   「产量」对「年累计产量」是 1/4，够进入候选，不够独自定案（见证据门槛）。
///
/// 注意分母是标签而不是问题：问题里多说几个字（「各产线的」）不该稀释证据。
pub fn label_coverage(qn: &str, q_tokens: &BTreeSet<String>, labels: &[String]) -> f64 {
    let mut best: f64 = 0.0;
    for lab in labels {
        let p = normalize(lab);
        if p.is_empty() {
            continue;
        }
        if find_span(qn, &p, &[]).is_some() {
            return 1.0;
        }
        let lt = tokens_of(&p);
        if lt.is_empty() {
            continue;
        }
        let hit = lt.iter().filter(|t| q_tokens.contains(*t)).count();
        best = best.max(hit as f64 / lt.len() as f64);
    }
    best
}

/// 两个 token 集的 Jaccard 相似度。
pub fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.iter().filter(|t| b.contains(*t)).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        return 0.0;
    }
    inter as f64 / union as f64
}

/// 把一组分数线性缩放到 [0,1]。全平的一组映射成全 0——没有区分度就是没有信号，
/// 不该在融合里冒充成半票。
pub fn minmax(in_: &BTreeMap<String, f64>) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    if in_.is_empty() {
        return out;
    }
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for &v in in_.values() {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    let span = hi - lo;
    for (k, &v) in in_ {
        out.insert(k.clone(), if span == 0.0 { 0.0 } else { (v - lo) / span });
    }
    out
}

/// 召回前 `top_k` 个指标。
///
/// 接了 [`Recall`] 就是混合召回（词法 ⊕ 语义，权重 0.4/0.6，语义主导，因为
/// 同义词写不全是常态）；没接就是纯词法。两种情况下**逐字命中都是分数下界**：
/// 语义模型有权认为「营收」和「产能利用率」都挺像，但没有权把一个逐字出现的
/// 同义词压到候选之外。
pub fn retrieve(
    model: &Model,
    question: &str,
    top_k: usize,
    recall: Option<&dyn Recall>,
) -> Vec<ScoredMetric> {
    let lex = lexical_scores(model, question);

    let mut dense: BTreeMap<String, f64> = BTreeMap::new();
    if let Some(r) = recall {
        let docs: Vec<String> = model.metrics.iter().map(metric_doc).collect();
        let sims = r.similarity(question, &docs);
        // 长度不符说明实现方没守约定，忽略这个信号，退化到词法。
        if sims.len() == docs.len() {
            for (m, s) in model.metrics.iter().zip(sims) {
                dense.insert(m.name.clone(), s);
            }
        }
    }

    let (lex_n, dense_n) = (minmax(&lex), minmax(&dense));
    const W_LEX: f64 = 0.4;
    const W_DENSE: f64 = 0.6;

    // 按模型定义序构建，排序用稳定排序：同分时的先后是可复现的。
    let mut out: Vec<ScoredMetric> = Vec::new();
    for m in &model.metrics {
        let raw = lex.get(&m.name).copied().unwrap_or(0.0);
        let score = if dense.is_empty() {
            raw
        } else {
            let fused = W_LEX * lex_n.get(&m.name).copied().unwrap_or(0.0)
                + W_DENSE * dense_n.get(&m.name).copied().unwrap_or(0.0);
            fused.max(raw)
        };
        if score > 0.0 {
            out.push(ScoredMetric {
                name: m.name.clone(),
                score,
            });
        }
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out.truncate(top_k);
    rerank(model, question, out)
}

/// cross-encoder 重排：检索分仍然定调，但一个逐字命中的名字/同义词可以反超一个
/// 检索分更高的邻居。权重不需要求和为 1，重要的是相对量级。
fn rerank(model: &Model, question: &str, cands: Vec<ScoredMetric>) -> Vec<ScoredMetric> {
    if cands.len() < 2 {
        return cands;
    }
    const W_RETRIEVAL: f64 = 0.45;
    const W_LEXICAL: f64 = 0.35;
    const W_OVERLAP: f64 = 0.20;

    let qn = normalize(question);
    let q_tokens = tokens_of(question);

    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for c in &cands {
        lo = lo.min(c.score);
        hi = hi.max(c.score);
    }
    let norm = |s: f64| if hi == lo { 1.0 } else { (s - lo) / (hi - lo) };

    let mut out: Vec<ScoredMetric> = cands
        .iter()
        .map(|c| {
            let (lex, overlap) = match model.metric(&c.name) {
                Some(m) => (
                    label_coverage(&qn, &q_tokens, &metric_labels(m)),
                    jaccard(&q_tokens, &tokens_of(&normalize(&m.description))),
                ),
                None => (0.0, 0.0),
            };
            ScoredMetric {
                name: c.name.clone(),
                score: W_RETRIEVAL * norm(c.score) + W_LEXICAL * lex + W_OVERLAP * overlap,
            }
        })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out
}
