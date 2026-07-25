# boss-briefing · 老板内参

基于 **harness-rs** 的行业情报 MVP：采集 RSS/Atom，按时效、关键词、来源质量和风险信号去重排序，输出可追溯的 Markdown/JSON。配置 OpenAI-compatible 模型后，通过 `harness-models::OpenAiCompat` 增加 CEO 视角研判。

## 功能

- 多数据源采集，单源失败不影响整份报告
- 时间窗口过滤、URL跟踪参数清理、标题近似去重
- 关注词、竞品词、风险词匹配和可解释评分
- 风险预警 / 竞争动态 / 产品技术 / 资本动向 / 行业动态分类
- 无API Key时仍生成规则版；有模型时生成“老板先看”
- 输出带时间戳的 Markdown、JSON 和 `latest.md`

## 使用

```bash
cd /Users/liliang/Things/AI/base-rs/harness/projects/boss-briefing
cargo run -- init --output boss-briefing.toml
cargo run -- check --config boss-briefing.toml
cargo run -- run --config boss-briefing.toml --no-ai
```

启用AI：把配置中的 `ai.enabled` 改为 `true`，然后设置 `DEEPSEEK_API_KEY`；也可修改 `base_url/model/api_key_env` 接任何 OpenAI-compatible 服务。

## 本地演示

```bash
sed "s|REPLACE_WITH_ABSOLUTE_FIXTURE_PATH|$PWD/fixtures/sample.xml|" fixtures/sample.toml > /tmp/boss-briefing-demo.toml
cargo run -- run --config /tmp/boss-briefing-demo.toml --output /tmp/boss-briefing-out --no-ai --now 2026-07-23T15:15:00Z
```

## 产品化顺序

1. 先选一个行业交付 3–5 份样本，验证老板是否真的阅读。
2. 增加历史对比，只报告新增变化。
3. 增加企业微信、飞书、邮件推送。
4. 增加 Web 配置页和行业模板。
5. 用点击与人工反馈改进排序。
