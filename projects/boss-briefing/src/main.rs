use anyhow::{Context as _, bail};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use harness_core::{Block, Context, Model, Task, Turn, TurnRole};
use harness_models::OpenAiCompat;
use quick_xml::de::from_str;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(
    name = "boss-briefing",
    version,
    about = "采集行业资讯并生成可追溯的老板内参"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Run {
        #[arg(long, default_value = "boss-briefing.toml")]
        config: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        no_ai: bool,
        #[arg(long)]
        now: Option<String>,
    },
    Check {
        #[arg(long, default_value = "boss-briefing.toml")]
        config: PathBuf,
    },
    Init {
        #[arg(long, default_value = "boss-briefing.toml")]
        output: PathBuf,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    briefing: BriefingConfig,
    #[serde(default)]
    ranking: RankingConfig,
    #[serde(default)]
    ai: AiConfig,
    #[serde(default)]
    sources: Vec<SourceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BriefingConfig {
    title: String,
    industry: String,
    #[serde(default = "default_audience")]
    audience: String,
    #[serde(default = "default_lookback")]
    lookback_hours: i64,
    #[serde(default = "default_max_items")]
    max_items: usize,
    #[serde(default = "default_output_dir")]
    output_dir: PathBuf,
    #[serde(default)]
    focus_keywords: Vec<String>,
    #[serde(default)]
    competitor_keywords: Vec<String>,
    #[serde(default)]
    risk_keywords: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RankingConfig {
    #[serde(default = "default_recency_weight")]
    recency_weight: f64,
    #[serde(default = "default_keyword_weight")]
    keyword_weight: f64,
    #[serde(default = "default_source_weight")]
    source_weight: f64,
    #[serde(default = "default_risk_bonus")]
    risk_bonus: f64,
}
impl Default for RankingConfig {
    fn default() -> Self {
        Self {
            recency_weight: 35.0,
            keyword_weight: 35.0,
            source_weight: 20.0,
            risk_bonus: 10.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AiConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_base_url")]
    base_url: String,
    #[serde(default = "default_model")]
    model: String,
    #[serde(default = "default_api_key_env")]
    api_key_env: String,
    #[serde(default = "default_ai_max_items")]
    max_items: usize,
}
impl Default for AiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: default_base_url(),
            model: default_model(),
            api_key_env: default_api_key_env(),
            max_items: 12,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourceConfig {
    name: String,
    url: String,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default = "default_priority")]
    priority: f64,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Article {
    title: String,
    url: String,
    summary: String,
    source: String,
    published_at: Option<DateTime<Utc>>,
    source_priority: f64,
    source_tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct RankedArticle {
    #[serde(flatten)]
    article: Article,
    score: f64,
    matched_keywords: Vec<String>,
    risk_signals: Vec<String>,
    category: String,
}

#[derive(Debug, Clone, Serialize)]
struct SourceFailure {
    source: String,
    error: String,
}

#[derive(Debug, Deserialize)]
struct Rss {
    channel: RssChannel,
}
#[derive(Debug, Deserialize)]
struct RssChannel {
    #[serde(rename = "item", default)]
    items: Vec<RssItem>,
}
#[derive(Debug, Deserialize)]
struct RssItem {
    title: Option<String>,
    link: Option<String>,
    description: Option<String>,
    #[serde(rename = "pubDate")]
    pub_date: Option<String>,
}
#[derive(Debug, Deserialize)]
struct AtomFeed {
    #[serde(rename = "entry", default)]
    entries: Vec<AtomEntry>,
}
#[derive(Debug, Deserialize)]
struct AtomEntry {
    title: Option<String>,
    summary: Option<String>,
    content: Option<String>,
    published: Option<String>,
    updated: Option<String>,
    #[serde(rename = "link", default)]
    links: Vec<AtomLink>,
}
#[derive(Debug, Deserialize)]
struct AtomLink {
    #[serde(rename = "@href")]
    href: Option<String>,
    #[serde(rename = "@rel")]
    rel: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Init { output } => init_config(&output),
        Command::Check { config } => {
            let cfg = load_config(&config)?;
            validate(&cfg)?;
            println!("✓ 配置有效：{}", config.display());
            println!("  行业：{}", cfg.briefing.industry);
            println!(
                "  数据源：{} 个",
                cfg.sources.iter().filter(|s| s.enabled).count()
            );
            for s in cfg.sources.iter().filter(|s| s.enabled) {
                println!("    - {} ({})", s.name, s.url);
            }
            println!("  AI：{}", if cfg.ai.enabled { "启用" } else { "关闭" });
            Ok(())
        }
        Command::Run {
            config,
            output,
            no_ai,
            now,
        } => run(&config, output, no_ai, now.as_deref()).await,
    }
}

async fn run(
    path: &Path,
    output: Option<PathBuf>,
    no_ai: bool,
    fixed_now: Option<&str>,
) -> anyhow::Result<()> {
    let mut cfg = load_config(path)?;
    validate(&cfg)?;
    if let Some(out) = output {
        cfg.briefing.output_dir = out;
    }
    if no_ai {
        cfg.ai.enabled = false;
    }
    let now = fixed_now
        .map(DateTime::parse_from_rfc3339)
        .transpose()
        .context("--now 必须是 RFC3339 时间")?
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    println!("→ 老板内参：{}", cfg.briefing.industry);
    let (articles, failures, raw_count) = collect_all(&cfg, now).await;
    for f in &failures {
        eprintln!("  ⚠ {}：{}", f.source, f.error);
    }
    if articles.is_empty() {
        bail!("没有采集到可用资讯，请检查网络、时间窗口和数据源");
    }
    let ranked = rank_and_deduplicate(articles, &cfg, now);
    println!("  原始 {raw_count} 条 → 去重筛选后 {} 条", ranked.len());
    let analysis = if cfg.ai.enabled {
        match analyze_with_harness(&cfg, &ranked, now).await {
            Ok(text) => {
                println!("  ✓ AI研判完成（harness-rs / {}）", cfg.ai.model);
                Some(text)
            }
            Err(e) => {
                eprintln!("  ⚠ AI研判失败，降级为规则版：{e:#}");
                None
            }
        }
    } else {
        None
    };
    let paths = generate_report(&cfg, &ranked, &failures, analysis, now)?;
    println!(
        "✓ 已生成\n  Markdown：{}\n  JSON：{}\n  最新版：{}",
        paths.0.display(),
        paths.1.display(),
        paths.2.display()
    );
    Ok(())
}

fn load_config(path: &Path) -> anyhow::Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("读取配置失败：{}", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("解析配置失败：{}", path.display()))
}

fn validate(cfg: &Config) -> anyhow::Result<()> {
    if cfg.briefing.title.trim().is_empty() || cfg.briefing.industry.trim().is_empty() {
        bail!("title 和 industry 不能为空");
    }
    if cfg.briefing.lookback_hours <= 0 || cfg.briefing.max_items == 0 {
        bail!("lookback_hours 和 max_items 必须大于0");
    }
    if !cfg.sources.iter().any(|s| s.enabled) {
        bail!("至少启用一个数据源");
    }
    for s in cfg.sources.iter().filter(|s| s.enabled) {
        let url = url::Url::parse(&s.url).with_context(|| format!("无效URL：{}", s.url))?;
        if !matches!(url.scheme(), "http" | "https" | "file") {
            bail!("不支持的数据源协议：{}", s.url);
        }
    }
    Ok(())
}

async fn collect_all(
    cfg: &Config,
    now: DateTime<Utc>,
) -> (Vec<Article>, Vec<SourceFailure>, usize) {
    let client = reqwest::Client::builder()
        .user_agent(concat!("boss-briefing/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .unwrap();
    let mut articles = Vec::new();
    let mut failures = Vec::new();
    for source in cfg.sources.iter().filter(|s| s.enabled) {
        let result = async {
            let body = if let Some(p) = source.url.strip_prefix("file://") {
                tokio::fs::read_to_string(p).await?
            } else {
                client
                    .get(&source.url)
                    .send()
                    .await?
                    .error_for_status()?
                    .text()
                    .await?
            };
            parse_feed(&body, source)
        }
        .await;
        match result {
            Ok(mut v) => articles.append(&mut v),
            Err(e) => failures.push(SourceFailure {
                source: source.name.clone(),
                error: format!("{e:#}"),
            }),
        }
    }
    let raw = articles.len();
    let cutoff = now - chrono::Duration::hours(cfg.briefing.lookback_hours);
    articles.retain(|a| a.published_at.is_none_or(|d| d >= cutoff));
    (articles, failures, raw)
}

fn parse_feed(body: &str, source: &SourceConfig) -> anyhow::Result<Vec<Article>> {
    if let Ok(rss) = from_str::<Rss>(body)
        && !rss.channel.items.is_empty()
    {
        return Ok(rss
            .channel
            .items
            .into_iter()
            .filter_map(|i| {
                let title = clean(i.title.as_deref()?);
                let url = i.link?.trim().to_string();
                (!title.is_empty() && !url.is_empty()).then(|| Article {
                    title,
                    url,
                    summary: clean(i.description.as_deref().unwrap_or("")),
                    source: source.name.clone(),
                    published_at: i.pub_date.as_deref().and_then(parse_date),
                    source_priority: source.priority.clamp(0.0, 1.0),
                    source_tags: source.tags.clone(),
                })
            })
            .collect());
    }
    let atom: AtomFeed = from_str(body)?;
    Ok(atom
        .entries
        .into_iter()
        .filter_map(|e| {
            let title = clean(e.title.as_deref()?);
            let url = e
                .links
                .iter()
                .find(|l| l.rel.as_deref().is_none_or(|r| r == "alternate"))
                .or_else(|| e.links.first())?
                .href
                .clone()?;
            Some(Article {
                title,
                url,
                summary: clean(e.summary.as_deref().or(e.content.as_deref()).unwrap_or("")),
                source: source.name.clone(),
                published_at: e
                    .published
                    .as_deref()
                    .or(e.updated.as_deref())
                    .and_then(parse_date),
                source_priority: source.priority.clamp(0.0, 1.0),
                source_tags: source.tags.clone(),
            })
        })
        .collect())
}

fn parse_date(v: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc2822(v)
        .or_else(|_| DateTime::parse_from_rfc3339(v))
        .map(|d| d.with_timezone(&Utc))
        .ok()
}
fn clean(v: &str) -> String {
    regex::Regex::new(r"(?s)<[^>]*>")
        .unwrap()
        .replace_all(v, " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn rank_and_deduplicate(
    articles: Vec<Article>,
    cfg: &Config,
    now: DateTime<Utc>,
) -> Vec<RankedArticle> {
    let mut ranked: Vec<_> = articles.into_iter().map(|a| score(a, cfg, now)).collect();
    ranked.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    let mut urls = HashSet::new();
    let mut titles = Vec::<String>::new();
    ranked.retain(|item| {
        let u = canonical_url(&item.article.url);
        let t = normalize_title(&item.article.title);
        if !urls.insert(u) || titles.iter().any(|seen| similarity(seen, &t) >= 0.82) {
            return false;
        }
        titles.push(t);
        true
    });
    ranked.truncate(cfg.briefing.max_items);
    ranked
}

fn score(article: Article, cfg: &Config, now: DateTime<Utc>) -> RankedArticle {
    let hay = format!(
        "{} {} {}",
        article.title,
        article.summary,
        article.source_tags.join(" ")
    )
    .to_lowercase();
    let mut matched = keyword_matches(&hay, &cfg.briefing.focus_keywords);
    let competitors = keyword_matches(&hay, &cfg.briefing.competitor_keywords);
    matched.extend(competitors.clone());
    matched.sort();
    matched.dedup();
    let risks = keyword_matches(&hay, &cfg.briefing.risk_keywords);
    let age = article
        .published_at
        .map(|d| (now - d).num_minutes().max(0) as f64 / 60.0)
        .unwrap_or(cfg.briefing.lookback_hours as f64 * 0.75);
    let recency = (1.0 - age / cfg.briefing.lookback_hours as f64).clamp(0.0, 1.0);
    let score = recency * cfg.ranking.recency_weight
        + (matched.len() as f64 / 4.0).min(1.0) * cfg.ranking.keyword_weight
        + article.source_priority * cfg.ranking.source_weight
        + if risks.is_empty() {
            0.0
        } else {
            cfg.ranking.risk_bonus
        };
    let category = if !risks.is_empty() {
        "风险预警"
    } else if !competitors.is_empty() {
        "竞争动态"
    } else if has(&hay, &["融资", "funding", "acquisition", "并购", "投资"]) {
        "资本动向"
    } else if has(&hay, &["发布", "launch", "release", "开源", "模型"]) {
        "产品技术"
    } else {
        "行业动态"
    }
    .into();
    RankedArticle {
        article,
        score,
        matched_keywords: matched,
        risk_signals: risks,
        category,
    }
}
fn keyword_matches(hay: &str, words: &[String]) -> Vec<String> {
    words
        .iter()
        .filter(|w| !w.trim().is_empty() && hay.contains(&w.to_lowercase()))
        .cloned()
        .collect()
}
fn has(hay: &str, words: &[&str]) -> bool {
    words.iter().any(|w| hay.contains(w))
}
fn canonical_url(raw: &str) -> String {
    let Ok(mut u) = url::Url::parse(raw) else {
        return raw.trim_end_matches('/').to_lowercase();
    };
    let keep: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !k.starts_with("utm_") && !matches!(k.as_ref(), "gclid" | "fbclid"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    u.set_query(None);
    if !keep.is_empty() {
        u.query_pairs_mut().extend_pairs(keep);
    }
    u.set_fragment(None);
    u.to_string().trim_end_matches('/').to_lowercase()
}
fn normalize_title(t: &str) -> String {
    t.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || ('\u{4e00}'..='\u{9fff}').contains(&c) {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let aa: HashSet<_> = a.split_whitespace().collect();
    let bb: HashSet<_> = b.split_whitespace().collect();
    let token_score = if aa.is_empty() || bb.is_empty() {
        0.0
    } else {
        aa.intersection(&bb).count() as f64 / aa.union(&bb).count() as f64
    };
    let char_score = jaccard(&char_bigrams(a), &char_bigrams(b));
    let prefix_score = common_prefix_ratio(a, b);
    token_score.max(char_score).max(prefix_score)
}

fn common_prefix_ratio(a: &str, b: &str) -> f64 {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let shorter = a_chars.len().min(b_chars.len());
    if shorter == 0 {
        return 0.0;
    }
    let common = a_chars
        .iter()
        .zip(&b_chars)
        .take_while(|(left, right)| left == right)
        .count();
    common as f64 / shorter as f64
}

fn char_bigrams(value: &str) -> HashSet<String> {
    let chars: Vec<char> = value.chars().filter(|c| !c.is_whitespace()).collect();
    chars
        .windows(2)
        .map(|pair| pair.iter().collect::<String>())
        .collect()
}

fn jaccard<T: Eq + std::hash::Hash>(a: &HashSet<T>, b: &HashSet<T>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    a.intersection(b).count() as f64 / a.union(b).count() as f64
}

async fn analyze_with_harness(
    cfg: &Config,
    articles: &[RankedArticle],
    now: DateTime<Utc>,
) -> anyhow::Result<String> {
    let key = std::env::var(&cfg.ai.api_key_env)
        .with_context(|| format!("环境变量 {} 未设置", cfg.ai.api_key_env))?;
    let model = OpenAiCompat::with_key(&cfg.ai.base_url, &cfg.ai.model, key);
    let evidence = articles
        .iter()
        .take(cfg.ai.max_items)
        .enumerate()
        .map(|(i, a)| {
            format!(
                "[{}] {}\n来源:{} 时间:{} 规则分:{:.1}\n摘要:{}\n链接:{}",
                i + 1,
                a.article.title,
                a.article.source,
                a.article
                    .published_at
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_else(|| "未知".into()),
                a.score,
                clip(&a.article.summary, 500),
                a.article.url
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let system = format!(
        "你是{}行业的CEO情报顾问，读者是{}。只根据提供的证据，不能编造。输出中文Markdown，900字内，固定包含：### 一句话判断；### 最值得老板关注的三件事（每项写发生了什么/为什么重要/建议动作，并引用[编号]）；### 风险雷达；### 未来7天观察清单。",
        cfg.briefing.industry, cfg.briefing.audience
    );
    let question = format!(
        "生成截至 {} 的老板内参研判。\n\n{}",
        now.to_rfc3339(),
        evidence
    );
    let ctx = Context {
        system: vec![Block::Text(system)],
        guides: vec![],
        history: vec![Turn {
            role: TurnRole::User,
            blocks: vec![Block::Text(question.clone())],
        }],
        task: Task {
            description: question,
            source: None,
            deadline: None,
        },
        policy: Default::default(),
        metadata: BTreeMap::new(),
        tools: vec![],
        response_format: Default::default(),
    };
    model
        .complete(&ctx)
        .await?
        .text
        .filter(|t| !t.trim().is_empty())
        .context("模型返回空文本")
}

fn generate_report(
    cfg: &Config,
    articles: &[RankedArticle],
    failures: &[SourceFailure],
    ai: Option<String>,
    now: DateTime<Utc>,
) -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    std::fs::create_dir_all(&cfg.briefing.output_dir)?;
    let stamp = now.format("%Y-%m-%d_%H%M");
    let md = cfg.briefing.output_dir.join(format!("briefing-{stamp}.md"));
    let json = cfg
        .briefing
        .output_dir
        .join(format!("briefing-{stamp}.json"));
    let latest = cfg.briefing.output_dir.join("latest.md");
    let text = render_markdown(cfg, articles, failures, ai.as_deref(), now);
    std::fs::write(&md, &text)?;
    std::fs::write(&latest, &text)?;
    #[derive(Serialize)]
    struct Payload<'a> {
        title: &'a str,
        industry: &'a str,
        generated_at: DateTime<Utc>,
        ai_analysis: &'a Option<String>,
        articles: &'a [RankedArticle],
        source_failures: &'a [SourceFailure],
    }
    std::fs::write(
        &json,
        serde_json::to_string_pretty(&Payload {
            title: &cfg.briefing.title,
            industry: &cfg.briefing.industry,
            generated_at: now,
            ai_analysis: &ai,
            articles,
            source_failures: failures,
        })?,
    )?;
    Ok((md, json, latest))
}
fn render_markdown(
    cfg: &Config,
    articles: &[RankedArticle],
    failures: &[SourceFailure],
    ai: Option<&str>,
    now: DateTime<Utc>,
) -> String {
    let mut out = format!(
        "# {}\n\n> **行业：** {}　**读者：** {}　**生成时间：** {} UTC\n> \n> 筛选 **{}** 条高价值资讯；所有判断均保留原始链接。\n\n## 老板先看\n\n",
        cfg.briefing.title,
        cfg.briefing.industry,
        cfg.briefing.audience,
        now.format("%Y-%m-%d %H:%M"),
        articles.len()
    );
    if let Some(a) = ai {
        out.push_str(a.trim());
    } else {
        out.push_str("未启用AI研判，规则排序前三：\n\n");
        for (i, a) in articles.iter().take(3).enumerate() {
            out.push_str(&format!(
                "{}. **{}**（{}，{:.1}分）\n",
                i + 1,
                a.article.title,
                a.category,
                a.score
            ));
        }
    }
    out.push_str("\n\n");
    for category in ["风险预警", "竞争动态", "产品技术", "资本动向", "行业动态"]
    {
        let items: Vec<_> = articles
            .iter()
            .enumerate()
            .filter(|(_, a)| a.category == category)
            .collect();
        if items.is_empty() {
            continue;
        }
        out.push_str(&format!("## {category}\n\n"));
        for (i, a) in items {
            out.push_str(&format!(
                "### [{}] {}\n\n- **来源：** {}",
                i + 1,
                a.article.title,
                a.article.source
            ));
            if let Some(d) = a.article.published_at {
                out.push_str(&format!(" · {} UTC", d.format("%m-%d %H:%M")));
            }
            out.push_str(&format!(" · 情报分 **{:.1}**\n", a.score));
            if !a.article.summary.is_empty() {
                out.push_str(&format!("- **摘要：** {}\n", clip(&a.article.summary, 360)));
            }
            if !a.matched_keywords.is_empty() {
                out.push_str(&format!("- **命中：** {}\n", a.matched_keywords.join("、")));
            }
            out.push_str(&format!("- **原文：** {}\n\n", a.article.url));
        }
    }
    if !failures.is_empty() {
        out.push_str("## 数据质量说明\n\n");
        for f in failures {
            out.push_str(&format!("- {}：{}\n", f.source, f.error));
        }
        out.push('\n');
    }
    out.push_str("---\n*由 boss-briefing / harness-rs 生成；用于经营判断辅助，不替代原文核验。*\n");
    out
}
fn clip(v: &str, n: usize) -> String {
    let s = v.chars().take(n).collect::<String>();
    if v.chars().count() > n {
        format!("{s}…")
    } else {
        s
    }
}
fn init_config(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        bail!("文件已存在，不覆盖：{}", path.display());
    }
    if let Some(p) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(path, include_str!("../boss-briefing.example.toml"))?;
    println!("✓ 已创建 {}", path.display());
    Ok(())
}
fn default_audience() -> String {
    "CEO/创始人".into()
}
fn default_lookback() -> i64 {
    72
}
fn default_max_items() -> usize {
    12
}
fn default_output_dir() -> PathBuf {
    PathBuf::from("out")
}
fn default_recency_weight() -> f64 {
    35.0
}
fn default_keyword_weight() -> f64 {
    35.0
}
fn default_source_weight() -> f64 {
    20.0
}
fn default_risk_bonus() -> f64 {
    10.0
}
fn default_base_url() -> String {
    "https://api.deepseek.com".into()
}
fn default_model() -> String {
    "deepseek-chat".into()
}
fn default_api_key_env() -> String {
    "DEEPSEEK_API_KEY".into()
}
fn default_ai_max_items() -> usize {
    12
}
fn yes() -> bool {
    true
}
fn default_priority() -> f64 {
    0.7
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source() -> SourceConfig {
        SourceConfig {
            name: "测试".into(),
            url: "file:///tmp/x".into(),
            enabled: true,
            priority: 0.8,
            tags: vec!["AI".into()],
        }
    }
    #[test]
    fn parses_rss() {
        let xml = r#"<rss><channel><item><title>AI &amp; SaaS</title><link>https://a.com/x</link><description><![CDATA[<b>收入增长</b> 40%]]></description><pubDate>Thu, 23 Jul 2026 10:00:00 GMT</pubDate></item></channel></rss>"#;
        let a = parse_feed(xml, &source()).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].title, "AI & SaaS");
        assert_eq!(a[0].summary, "收入增长 40%");
    }
    #[test]
    fn canonical_dedup() {
        assert_eq!(
            canonical_url("https://a.com/x?utm_source=a"),
            canonical_url("https://a.com/x?utm_source=b")
        );
    }
    #[test]
    fn similar_titles() {
        assert!(similarity("openai 发布 agent", "openai 发布 agent") > 0.9);
        assert!(
            similarity(
                "ai办公进入智能体时代 腾讯阿里金山办公等加速抢入口 半年超20款产品入场 sohu",
                "ai办公进入智能体时代 腾讯阿里金山办公等加速抢入口 半年超20款产品入场 新浪财经"
            ) > 0.82
        );
    }
}
