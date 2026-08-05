//! 分词与短语匹配 —— 这一层单独存在，是因为中文不写空格。
//!
//! 一个按空白切词的匹配器读「各门店大区的营收」只会得到一个 token：整句话。
//! 于是「营收」这个同义词永远匹配不上，词法召回在中文问题上不是变差，是直接归零，
//! 而上层看到的现象只是「没找到指标」，看不出原因出在切词。
//!
//! 这里的做法是不引入分词器：ASCII 走词，CJK 走**重叠二元组**。
//! 「各门店大区的营收」→ 各门,门店,店大,大区,区的,的营,营收。切错的那几个
//! (店大、区的) 只是噪声，不会命中任何指标名；切对的那几个 (门店、大区、营收) 足以
//! 让 BM25 式的重叠打分工作。用一个不完美但**确定**的切法，换掉一个需要词典、
//! 需要更新、且在业务黑话上一样会切错的分词器。

use std::collections::BTreeSet;

/// 英文问句里高频但不携带业务含义的词。留着它们会让「revenue by region」和
/// 「revenue by product」在重叠打分上互相靠近。
const STOP: &[&str] = &[
    "the", "a", "an", "of", "for", "by", "in", "to", "and", "what", "is", "are", "how", "many",
    "show", "me", "per", "each",
];

/// 是否属于「不写空格」的东亚文字。
///
/// 这里手写码位区间而不是依赖 unicode 属性表：判定只服务于「要不要切二元组」，
/// 边界上多一个少一个汉字不改变召回结果，而多一个 unicode 表依赖要拖进整个
/// 语义层的构建。
pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3400..=0x4DBF        // 汉字扩展 A
        | 0x4E00..=0x9FFF      // 汉字基本区
        | 0xF900..=0xFAFF      // 兼容汉字
        | 0x20000..=0x2FA1F    // 汉字扩展 B 及以上
        | 0x3040..=0x309F      // 平假名
        | 0x30A0..=0x30FF      // 片假名
        | 0x31F0..=0x31FF
        | 0xFF66..=0xFF9D      // 半角片假名
        | 0x1100..=0x11FF      // 谚文字母
        | 0x3130..=0x318F
        | 0xAC00..=0xD7AF      // 谚文音节
    )
}

/// 字符串里是否含 CJK 字符。
pub fn has_cjk(s: &str) -> bool {
    s.chars().any(is_cjk)
}

/// 把一段文本切成 token 集合：ASCII 的 `[a-z0-9]+` 连续段（长度 >1、非停用词），
/// 加上 CJK 的重叠二元组（单独一个 CJK 字符退化成一元组）。
///
/// 返回有序集合而不是哈希集合：下游的 [`fts_query`] 会把它拼成字符串，
/// 顺序不定就意味着同一个问题两次跑出两个查询串，测试和缓存都无从谈起。
pub fn tokens_of(s: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();

    let lower = s.to_lowercase();
    let mut word = String::new();
    for ch in lower.chars() {
        if ch.is_ascii_alphanumeric() {
            word.push(ch);
        } else {
            flush_word(&mut out, &mut word);
        }
    }
    flush_word(&mut out, &mut word);

    let mut run: Vec<char> = Vec::new();
    for ch in s.chars() {
        if is_cjk(ch) {
            run.push(ch);
        } else {
            flush_run(&mut out, &mut run);
        }
    }
    flush_run(&mut out, &mut run);

    out
}

fn flush_word(out: &mut BTreeSet<String>, word: &mut String) {
    // 长度 1 的 ASCII 片段（"a"、"3"）在任何模型里都不是证据。
    if word.chars().count() > 1 && !STOP.contains(&word.as_str()) {
        out.insert(std::mem::take(word));
    } else {
        word.clear();
    }
}

fn flush_run(out: &mut BTreeSet<String>, run: &mut Vec<char>) {
    if run.len() == 1 {
        out.insert(run[0].to_string());
    }
    for w in run.windows(2) {
        out.insert(w.iter().collect());
    }
    run.clear();
}

/// 把问题渲染成一条 FTS/BM25 的 OR 查询。
///
/// 本 crate 自己不带全文索引，这个函数是给接索引的那一天用的契约：
/// 交给 FTS 引擎的必须是**已经切好的 token**，因为 sqlite 的默认分词器会把
/// 一整句中文当成一个词，索引侧和查询侧各切各的，中文问题就永远召回不到东西。
pub fn fts_query(q: &str) -> String {
    let toks = tokens_of(q);
    if toks.is_empty() {
        return q.trim().to_string();
    }
    toks.into_iter().collect::<Vec<_>>().join(" OR ")
}

/// 归一化一个名字或问题：小写，下划线当空格。
/// `tea_revenue` 与 "tea revenue" 是同一个说法，模型里写哪种都不该影响匹配。
pub fn normalize(s: &str) -> String {
    s.to_lowercase().replace('_', " ")
}

/// 在 `q` 中找 `phrase` 的起始字节位置：要求落在词边界上，且不与已被占用的
/// 区间重叠，否则继续往后找。找不到返回 `None`。
///
/// 词边界只对 ASCII 生效——UTF-8 里 CJK 的后续字节都 ≥ 0x80，天然不是词字符，
/// 所以中文短语等价于子串匹配（本来也没有边界可言），而英文的 "region" 不会
/// 命中 "regional"。
pub fn find_span(q: &str, phrase: &str, consumed: &[bool]) -> Option<usize> {
    if phrase.is_empty() || phrase.len() > q.len() {
        return None;
    }
    let (qb, pb) = (q.as_bytes(), phrase.as_bytes());
    for at in 0..=(qb.len() - pb.len()) {
        if &qb[at..at + pb.len()] != pb {
            continue;
        }
        let end = at + pb.len();
        if word_boundary(qb, at, end) && !overlaps(consumed, at, end) {
            return Some(at);
        }
    }
    None
}

/// `phrase` 是否在 `q` 中出现（不考虑占用）。
pub fn has_phrase(q: &str, phrase: &str) -> bool {
    find_span(q, phrase, &[]).is_some()
}

/// 区间 `[at, end)` 是否与已占用的位置相交。
pub fn overlaps(consumed: &[bool], at: usize, end: usize) -> bool {
    consumed.iter().take(end).skip(at).any(|&c| c)
}

fn word_boundary(qb: &[u8], at: usize, end: usize) -> bool {
    if at > 0 && is_word_byte(qb[at - 1]) {
        return false;
    }
    if end < qb.len() && is_word_byte(qb[end]) {
        return false;
    }
    true
}

fn is_word_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}
