//! 落地 (grounding)：一个自然语言问题进来，一条语义查询出去 —— 或者一句反问。
//!
//! **这一层唯一的立场：宁可反问，不要猜。** 检索是按相似度排的，而在一个模型里
//! 所有指标本来就都在讲同一门生意，什么都跟什么有点像。「总电耗」召回成
//! 「吨件电耗」——一个求和变成了一个比率——排在第一位，置信度很高，后面还跟着
//! 一个真的算出来的数字。没有任何人会发现。所以这里把判定拆成两半：
//!
//! * **召回**允许模糊：向量、描述重叠，怎么宽都行，它只决定候选名单。
//! * **承诺**只认逐字证据：问题里确实出现了这个指标声明过的说法。证据并列时
//!   返回 [`Clarify`]，让人补一句话，而不是替他选一个。
//!
//! 第三件事是**粒度可行性**：「各茶类的营收」里的「营收」逐字命中 order 粒度的
//! revenue，但 order 到 tea 没有安全的连接路径——这个查询编译出来要么报错，要么
//! （在一个更宽松的编译器里）把总额沿着 tea 连接放大好几倍。所以词法命中之后还要
//! 过一遍 [`Model::dimensions_for`]：切不动就换一个切得动的（tea_revenue），
//! 换不出来就反问。

use crate::lexical::{ScoredMetric, label_coverage, metric_labels, retrieve};
use crate::recall::Recall;
use crate::tokens::{find_span, normalize, overlaps, tokens_of};
use harness_semantic::{Model, Query};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// 证据门槛：一个指标的名字/同义词被问题覆盖到这个比例以上，才够格独自定案。
///
/// 「产量」对同义词「年累计产量」是 1/4 —— 刚好够。低于此只算召回信号，不算承诺：
/// 一个长同义词被蹭到一个二元组，不是「用户说了这个指标」。
const EVIDENCE_FLOOR: f64 = 0.25;

/// 反问：问题在当前模型下有多于一个同样站得住的读法。
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Clarify {
    /// 给人看的一句话，直接可以显示。
    pub question: String,
    /// 候选名单。多数情况下是指标名；当分歧出在维度/时间轴上时是维度名。
    pub candidates: Vec<String>,
}

/// 落地结果，形状与 Go 版的 ground 对齐。
///
/// `clarify` 为 `Some` 时 `metrics` 一定为空 —— 「问一句」和「答一个」之间没有
/// 中间状态，一个半信半疑的查询照样会跑出一个数字来。
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Grounded {
    pub metrics: Vec<String>,
    pub group_by: Vec<String>,
    /// day | week | month | quarter | year，没识别到就是空。
    pub grain: String,
    /// 召回短名单，附带分数：给 LLM 提示词剪枝用，也是给人看的凭据。
    pub candidates: Vec<ScoredMetric>,
    pub clarify: Option<Clarify>,
}

impl Grounded {
    /// 是否落地成功（没有反问且有指标）。
    pub fn is_clear(&self) -> bool {
        self.clarify.is_none() && !self.metrics.is_empty()
    }

    /// 渲染成语义查询，交给 `harness_semantic::compile`。
    pub fn to_query(&self) -> Query {
        Query {
            metrics: self.metrics.clone(),
            group_by: self.group_by.clone(),
            time_grain: self.grain.clone(),
            ..Default::default()
        }
    }
}

/// 绑定在一个语义模型上的落地器。
///
/// 不打开数据库、不调用模型；给它一个问题，它只会读模型里的名字、同义词和连接图。
pub struct Grounder<'m> {
    model: &'m Model,
    top_k: usize,
    recall: Option<Box<dyn Recall>>,
}

impl<'m> Grounder<'m> {
    pub fn new(model: &'m Model) -> Self {
        Self {
            model,
            top_k: 8,
            recall: None,
        }
    }

    /// 候选名单的长度。只影响 `candidates`（凭据 / 提示词剪枝 / 反问时列几个），
    /// 不影响判定：词法证据是确定的，没有理由让一个 top-k 截断把它丢掉。
    pub fn with_top_k(mut self, k: usize) -> Self {
        self.top_k = k.max(1);
        self
    }

    /// 接上语义召回（向量/LLM）。不接就是纯词法，见 [`crate::recall`]。
    pub fn with_recall(mut self, r: Box<dyn Recall>) -> Self {
        self.recall = Some(r);
        self
    }

    pub fn model(&self) -> &Model {
        self.model
    }

    /// 当前的召回模式，写进回执用。
    pub fn mode(&self) -> &'static str {
        if self.recall.is_some() {
            "lexical(cjk)+recall+rerank"
        } else {
            "lexical(cjk)+rerank"
        }
    }

    /// 召回前 top-k 个指标。
    pub fn retrieve(&self, question: &str) -> Vec<ScoredMetric> {
        retrieve(
            self.model,
            question,
            self.top_k,
            self.recall.as_ref().map(|r| r.as_ref()),
        )
    }

    /// 把问题落成 `{metrics, group_by, grain}`，或给出反问。
    pub fn ground(&self, question: &str) -> Grounded {
        let qn = normalize(question);
        let q_tokens = tokens_of(question);
        let candidates = self.retrieve(question);

        // --- 1. 维度 ---------------------------------------------------
        // 先切维度再切指标，两套标签各占各的位置：「营收」不该被某个维度吃掉。
        let dim_labels = self.dim_labels();
        let (dim_hits, dim_conflicts) = claim(&qn, &dim_labels);
        if let Some(c) = dim_conflicts.first() {
            let names: Vec<String> = c.iter().map(|&i| self.model.dimensions[i].name.clone()).collect();
            return self.ask(
                candidates,
                format!("这个说法同时对应 {} 个维度，按哪个切？", names.len()),
                names,
            );
        }
        let mut group_by: Vec<String> = order_by_index(
            dim_hits,
            self.model.dimensions.iter().map(|d| d.name.clone()),
        );

        // --- 2. 时间粒度 -----------------------------------------------
        let grain = detect_grain(&qn);

        // --- 3. 指标 ---------------------------------------------------
        let metric_labels_all = self.metric_labels();
        let (metric_hits, metric_conflicts) = claim(&qn, &metric_labels_all);
        if let Some(c) = metric_conflicts.first() {
            // 两个指标声明了同一个说法，且同样权威。模型没能替用户区分，
            // 这里也不替他区分——选错的那次没有任何东西会发现。
            let names: Vec<String> = c.iter().map(|&i| self.model.metrics[i].name.clone()).collect();
            return self.ask(
                candidates,
                "这个说法在模型里对应多个指标，要看哪一个？".into(),
                names,
            );
        }
        let mut metrics: Vec<String> =
            order_by_index(metric_hits, self.model.metrics.iter().map(|m| m.name.clone()));

        // 逐字命中 = 用户点了名。没有点名时才退到部分证据，且部分证据必须唯一。
        let mut verbatim = !metrics.is_empty();
        if metrics.is_empty() {
            match self.best_partial(&qn, &q_tokens, &|_| true) {
                Pick::One(name) => metrics = vec![name],
                Pick::Tied(names) => {
                    return self.ask(
                        candidates,
                        "没听出确切要哪个指标，这几个都对得上一半，是哪个？".into(),
                        names,
                    );
                }
                Pick::None => {
                    let names = self.sliceable_names(&group_by, &candidates);
                    return self.ask(
                        candidates,
                        "没有匹配到指标。直接说出要看的那个度量，我就只答那一个。".into(),
                        names,
                    );
                }
            }
        }

        // --- 4. 粒度可行性 ---------------------------------------------
        let feasible: Vec<String> = metrics
            .iter()
            .filter(|m| self.can_slice(m, &group_by))
            .cloned()
            .collect();
        if feasible.is_empty() {
            // 点名的那个指标切不动这个维度。找一个切得动、且问题里也有证据的替代
            // ——「各茶类的营收」→ tea_revenue。找不到就把冲突如实说出来。
            let named = metrics.clone();
            let allow = |n: &str| !named.iter().any(|x| x == n) && self.can_slice(n, &group_by);
            match self.best_partial(&qn, &q_tokens, &allow) {
                Pick::One(name) => {
                    metrics = vec![name];
                    verbatim = false; // 替代品是部分证据得来的，不算点名
                }
                Pick::Tied(names) => {
                    return self.ask(
                        candidates,
                        format!(
                            "{} 不能按 {} 切分，这几个可以，要哪个？",
                            named.join("、"),
                            group_by.join("、")
                        ),
                        names,
                    );
                }
                Pick::None => {
                    let names = self.sliceable_names(&group_by, &candidates);
                    return self.ask(
                        candidates,
                        format!(
                            "{} 不能按 {} 切分（模型里没有不放大总数的连接路径）。换一个指标，或者换一个切法。",
                            named.join("、"),
                            group_by.join("、")
                        ),
                        names,
                    );
                }
            }
        } else {
            metrics = feasible;
        }

        // --- 5. 时间轴 -------------------------------------------------
        let has_time = self.has_time_dim(&group_by);
        if !has_time && grain.is_empty() && !verbatim {
            // 只对上一半证据的窗口指标（「产量」蹭到「年累计产量」），而问题里
            // 既没有时间维度也没有时间粒度：用户要的显然是它底下那个基础量。
            // 保留 window 只会编译失败（WindowNeedsTime），这不是更诚实，只是更晚。
            metrics = metrics.iter().map(|m| self.unwrap_window(m)).collect();
        }
        let needs_time = !grain.is_empty()
            || metrics
                .iter()
                .any(|m| self.model.metric(m).is_some_and(|x| x.is_window()));
        if needs_time && !has_time {
            let axes = self.shared_time_dims(&metrics);
            match axes.len() {
                1 => group_by.push(axes[0].clone()),
                _ => {
                    return self.ask(
                        candidates,
                        format!(
                            "要按时间看 {}，但{}。请指定时间维度。",
                            metrics.join("、"),
                            if axes.is_empty() {
                                "这个指标没有能安全连到的时间维度".to_string()
                            } else {
                                format!("有 {} 个时间维度可选", axes.len())
                            }
                        ),
                        axes,
                    );
                }
            }
        }
        group_by = order_names(&group_by, self.model.dimensions.iter().map(|d| &d.name));
        metrics = order_names(&metrics, self.model.metrics.iter().map(|m| &m.name));

        Grounded {
            metrics,
            group_by,
            grain,
            candidates,
            clarify: None,
        }
    }

    // --- 内部 ---------------------------------------------------------

    fn ask(&self, candidates: Vec<ScoredMetric>, question: String, mut names: Vec<String>) -> Grounded {
        // 十个名字是一个问题，四十四个是耸肩。
        names.truncate(10);
        Grounded {
            metrics: Vec::new(),
            group_by: Vec::new(),
            grain: String::new(),
            candidates,
            clarify: Some(Clarify { question, candidates: names }),
        }
    }

    fn metric_labels(&self) -> Vec<Label> {
        let mut out = Vec::new();
        for (i, m) in self.model.metrics.iter().enumerate() {
            out.push(Label::new(i, &m.name, RANK_NAME));
            for s in &m.synonyms {
                out.push(Label::new(i, s, RANK_SYNONYM));
            }
        }
        out
    }

    fn dim_labels(&self) -> Vec<Label> {
        let mut out = Vec::new();
        for (i, d) in self.model.dimensions.iter().enumerate() {
            out.push(Label::new(i, &d.name, RANK_NAME));
            for s in &d.synonyms {
                out.push(Label::new(i, s, RANK_SYNONYM));
            }
            // 物理列名排在最后：`plan_shift` 的列也叫 shift，但只有维度 `shift`
            // 是这个说法的正主。等价于模型自己的「名字压过同义词」规则。
            out.push(Label::new(i, &d.column, RANK_COLUMN));
        }
        out
    }

    /// 在允许的指标里找证据最强的那一个。并列（同分）就是并列，交给上层反问。
    fn best_partial(
        &self,
        qn: &str,
        q_tokens: &BTreeSet<String>,
        allow: &dyn Fn(&str) -> bool,
    ) -> Pick {
        let mut best = 0.0f64;
        let mut scored: Vec<(String, f64)> = Vec::new();
        for m in &self.model.metrics {
            if !allow(&m.name) {
                continue;
            }
            // 证据只看名字和同义词，不看描述：描述里出现「茶类」能帮助召回排序，
            // 但「描述里提过」不是「用户点了这个指标的名」。
            let cov = label_coverage(qn, q_tokens, &metric_labels(m));
            if cov >= EVIDENCE_FLOOR {
                best = best.max(cov);
                scored.push((m.name.clone(), cov));
            }
        }
        let top: Vec<String> = scored
            .into_iter()
            .filter(|(_, c)| (best - c).abs() < 1e-9)
            .map(|(n, _)| n)
            .collect();
        match top.len() {
            0 => Pick::None,
            1 => Pick::One(top.into_iter().next().unwrap()),
            _ => Pick::Tied(top),
        }
    }

    /// 指标能安全切分的维度。
    ///
    /// 窗口指标（年累计/滚动）在这里先展开成它累计的那个基础量：`dimensions_for`
    /// 认的是指标自己的 entity，而窗口指标没有 entity，直接问会得到「什么都不能切」。
    fn allowed_dims(&self, metric: &str) -> Vec<String> {
        let base = self.unwrap_window(metric);
        self.model.dimensions_for(&base).unwrap_or_default()
    }

    fn unwrap_window(&self, metric: &str) -> String {
        match self.model.metric(metric) {
            Some(m) if m.is_window() && !m.of.is_empty() => m.of.clone(),
            _ => metric.to_string(),
        }
    }

    fn can_slice(&self, metric: &str, group_by: &[String]) -> bool {
        if group_by.is_empty() {
            return self.model.metric(metric).is_some();
        }
        let allowed = self.allowed_dims(metric);
        group_by.iter().all(|d| allowed.iter().any(|a| a == d))
    }

    fn has_time_dim(&self, group_by: &[String]) -> bool {
        group_by
            .iter()
            .any(|d| self.model.dimension(d).is_some_and(|x| x.kind == "time"))
    }

    /// 这些指标**共同**能安全连到的时间维度。用交集是因为多指标查询里，
    /// 任何一个连不上的时间轴都会让整条查询失去意义。
    fn shared_time_dims(&self, metrics: &[String]) -> Vec<String> {
        let mut out: Option<Vec<String>> = None;
        for m in metrics {
            let axes: Vec<String> = self
                .allowed_dims(m)
                .into_iter()
                .filter(|d| self.model.dimension(d).is_some_and(|x| x.kind == "time"))
                .collect();
            out = Some(match out {
                None => axes,
                Some(prev) => prev.into_iter().filter(|p| axes.contains(p)).collect(),
            });
        }
        out.unwrap_or_default()
    }

    /// 反问时列哪些指标：能按当前维度切的那些（先按召回分排），实在没有就列全部。
    fn sliceable_names(&self, group_by: &[String], candidates: &[ScoredMetric]) -> Vec<String> {
        let mut out: Vec<String> = candidates
            .iter()
            .map(|c| c.name.clone())
            .filter(|n| self.can_slice(n, group_by))
            .collect();
        for m in &self.model.metrics {
            if !out.contains(&m.name) && self.can_slice(&m.name, group_by) {
                out.push(m.name.clone());
            }
        }
        if out.is_empty() {
            out = self.model.metric_names().iter().map(|s| s.to_string()).collect();
        }
        out
    }
}

enum Pick {
    One(String),
    Tied(Vec<String>),
    None,
}

// --- 短语占位 -----------------------------------------------------------

const RANK_NAME: u8 = 0;
const RANK_SYNONYM: u8 = 1;
const RANK_COLUMN: u8 = 2;

struct Label {
    owner: usize,
    phrase: String,
    rank: u8,
}

impl Label {
    fn new(owner: usize, phrase: &str, rank: u8) -> Self {
        Self {
            owner,
            phrase: normalize(phrase),
            rank,
        }
    }
}

/// 最长短语优先 + 区间占位：一个更具体的说法（「主机厂」）先占住自己的位置，
/// 一个与它重叠的泛称（「厂」）就不能在同一段文字上再命中一次。没有这一步，
/// 一句话会同时落到两个互相竞争的指标上，然后返回两列数。
///
/// 同一段文字上有多个同样权威的主张时（两个指标声明了同一个同义词），不选，
/// 记成冲突：模型没能替用户区分开的东西，这里也不替他区分。
fn claim(qn: &str, labels: &[Label]) -> (Vec<usize>, Vec<Vec<usize>>) {
    let mut by_len: BTreeMap<usize, Vec<&Label>> = BTreeMap::new();
    for l in labels {
        if !l.phrase.is_empty() {
            by_len.entry(l.phrase.len()).or_default().push(l);
        }
    }

    let mut consumed = vec![false; qn.len()];
    let mut chosen: Vec<usize> = Vec::new();
    let mut conflicts: Vec<Vec<usize>> = Vec::new();

    for (&len, group) in by_len.iter().rev() {
        // 同一轮（同样长度）里先把所有命中收齐，再按位置结算：这样「谁先声明」
        // 不会成为决胜条件。
        let mut hits: BTreeMap<usize, Vec<(usize, u8)>> = BTreeMap::new();
        for l in group {
            if chosen.contains(&l.owner) {
                continue;
            }
            if let Some(at) = find_span(qn, &l.phrase, &consumed) {
                let e = hits.entry(at).or_default();
                if !e.iter().any(|(o, _)| *o == l.owner) {
                    e.push((l.owner, l.rank));
                }
            }
        }
        for (at, owners) in hits {
            let end = at + len;
            if overlaps(&consumed, at, end) {
                continue; // 本轮更靠前的命中已经占走了
            }
            for c in consumed[at..end].iter_mut() {
                *c = true;
            }
            let best = owners.iter().map(|(_, r)| *r).min().unwrap_or(RANK_COLUMN);
            let top: Vec<usize> = owners
                .iter()
                .filter(|(_, r)| *r == best)
                .map(|(o, _)| *o)
                .collect();
            if top.len() == 1 {
                if !chosen.contains(&top[0]) {
                    chosen.push(top[0]);
                }
            } else {
                conflicts.push(top);
            }
        }
    }
    (chosen, conflicts)
}

// --- 时间粒度 -----------------------------------------------------------

/// 从问题里认时间粒度。
///
/// 只认成词的说法（「按月」「月度」「monthly」），不认单个「月」——「本月」
/// 「月饼销量」里的月不是粒度。认错一个粒度，出来的是一张行数完全不同的表。
fn detect_grain(qn: &str) -> String {
    const TABLE: &[(&str, &[&str])] = &[
        ("day", &["按天", "每天", "按日", "每日", "逐日", "日粒度", "daily", "by day", "per day"]),
        ("week", &["按周", "每周", "逐周", "周粒度", "weekly", "by week", "per week"]),
        ("month", &["按月", "每月", "逐月", "月度", "月粒度", "monthly", "by month", "per month"]),
        ("quarter", &["按季", "每季", "季度", "quarterly", "by quarter"]),
        ("year", &["按年", "每年", "逐年", "年度", "yearly", "annually", "by year", "per year"]),
    ];
    for (grain, words) in TABLE {
        for w in *words {
            if find_span(qn, w, &[]).is_some() {
                return (*grain).to_string();
            }
        }
    }
    String::new()
}

// --- 小工具 -------------------------------------------------------------

/// 按模型定义序输出被选中的下标对应的名字（去重）。
fn order_by_index(mut idx: Vec<usize>, names: impl Iterator<Item = String>) -> Vec<String> {
    idx.sort_unstable();
    idx.dedup();
    let all: Vec<String> = names.collect();
    idx.into_iter().filter_map(|i| all.get(i).cloned()).collect()
}

/// 把一组名字按模型定义序重排（去重）。落地结果的顺序不该取决于问题里先说了谁。
fn order_names<'a>(picked: &[String], all: impl Iterator<Item = &'a String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in all {
        if picked.contains(n) && !out.contains(n) {
            out.push(n.clone());
        }
    }
    for n in picked {
        if !out.contains(n) {
            out.push(n.clone());
        }
    }
    out
}
