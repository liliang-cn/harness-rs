# di-server — AI 战略顾问

连上公司的数据库,用自然语言问经营与战略问题,得到**能核查**的答案:每个数字都附着
真实执行过的那条查询。数据只读,全程留痕。

```sh
LLM_KEY=sk-... ./run.sh          # 打开 http://localhost:43200
```

首次运行会生成访问令牌并打印可点击的链接;浏览器里填 5 个字段连上数据库即可开始。
也有签名过的 macOS 桌面版(`desktop/`,双击即用)。

## 两种查数模式,按库自动切换

| | 直连模式 | 治理模式 |
|---|---|---|
| 触发条件 | 该库**没有**语义模型 | 该库有 `models/<库名>.yaml` |
| 模型能做什么 | `list_tables` / `describe_table` / `run_sql`(只读) | 只能 `list_metrics` / `get_dimensions` / `query_metric` |
| 优点 | 零建模,连上当天就能用 | 口径固定、跨 grain 编译正确、含 RBAC 与脱敏 |
| 代价 | 正确性依赖模型能力 | 需要先有语义模型(可一键生成后人工校) |

**门控写在工具代码里,不是提示词。** 治理库中 `run_sql` / `list_tables` /
`describe_table` 会直接返回失败并提示改用治理工具——模型物理上拿不到原始 SQL。
实测日志里模型两次尝试写 SQL 被拒,随后改走 `query_metric`。

治理能力本身来自 [DataIntelligence](https://github.com/liliang-cn/dataintelligence);
本项目是它的**客户端**,不重新实现语义层。

## 边界:这是产品,不是引擎

**di-server 不做治理引擎。** 语义编译、RBAC、脱敏、grounding、准确率闸门、版本灰度、
写回审批都在 DataIntelligence 里,通过 MCP 消费。

本项目只做 DI 刻意不做的事:

- **直连模式** — DI 必须要语义模型才能工作(给不存在的 `-model` 会直接退出),
  且只暴露 `query_metric`、没有 `run_sql`。零建模探索是这里补的。
- **入库前分支对比** — 复制受影响的表到分支 schema,在分支里入库,和生产比对
  行数与各数值列合计,人工确认后才合并。抓的是行级校验拦不住的整批错误:
  重复导入、金额单位从元变成分。
- **多引擎与切库** — PostgreSQL / MySQL / SQLite,界面里随时切换。
- **面向业务负责人的外壳** — 设置向导、令牌认证、图表与数据来源块、桌面应用。

DI 自己的 `ask`/`chat`/`copilot` 与 `/ui` 定位是 **CLI 与工程师运维台**,
不与本项目的产品界面竞争。

## 可信度

- **只读**:仅允许 `SELECT`/`WITH`,在只读事务里执行,500 行上限。
- **数据来源块**:回答末尾附本轮真实执行过的每条查询(行数、耗时、成败)。
  这段由**服务端**写入——模型能编数字就能编"证据",所以证据不能由模型产出。
- **hash 链审计**:每次请求留痕,可校验完整性。
- **模型可换**:默认走网关的 `gemini-3.6-flash-high`;要数据完全不出机器可换本地
  Ollama,但注意小模型会在 SQL 里编造系数(实测),本地部署建议配治理模式兜底。

## 结构

```
src/
  lib.rs       服务与路由(可被桌面壳复用)
  db.rs        多库接入,运行时切库
  dialect.rs   PG / MySQL / SQLite 的差异:引号、元数据来源、行转 JSON
  tools.rs     直连模式工具
  governed.rs  治理模式:DI 的 MCP 客户端(HTTP 或 stdio)
  branch.rs    入库前分支对比
  auth.rs      访问令牌     config.rs  连接配置     webui.rs  内嵌前端
ui/            React + @ai-gui(源码)   web/  构建产物(编进二进制)
models/        各库的语义模型 + endpoints.json(DI 的 HTTP 端点)
desktop/       Tauri 桌面壳(复用 lib.rs,不重写业务)
```

## 接入 DataIntelligence 的两种方式

**HTTP(生产)** — DI 独立部署,本进程只做客户端。不需要本机装 DI,也不用管子进程。

```jsonc
// models/endpoints.json
{ "conglomerate": { "url": "http://di-host:41955", "token": "finance-token" } }
```
```sh
di mcp -http :41955 -model models/conglomerate.yaml -dsn "$DSN" -role finance
```

**stdio(单机/开发)** — 没配端点时,按需拉起本机的 `di mcp` 子进程(需 `DI_BIN`)。

注意:DI 的服务配置是单模型 + 单数据源,**一个 DI 进程只服务一个库**;多个治理库需要
多个 DI 端点。

## 配置

| 变量 | 默认 | 说明 |
|---|---|---|
| `LLM_KEY` | — | 模型 API key(必填) |
| `LLM_MODEL` / `LLM_BASE` | `gemini-3.6-flash-high` / 网关 | 换模型 |
| `DI_SERVER_DSN` | 设置页保存的连接 | 直接指定数据库 |
| `DI_SERVER_TOKEN` | 首次运行生成 | 访问令牌 |
| `DI_MODELS` / `DI_BIN` / `DI_ROLE` | `models/` / 本机 di / `finance` | 治理模式 |
| `PORT` / `WEB_DIR` | `43200` / 内嵌 | 端口;`WEB_DIR` 设了就用磁盘上的前端 |

配置与令牌存放在 `~/.di-server/`(权限 0600)。

## 已知限制

- **入库分支只支持 PostgreSQL** — MySQL 的 schema 即 database、SQLite 是单文件,
  机制不同;在这两种库上调用会明确告知,而不是给出半对的结果。
- **单用户令牌**,没有用户体系与角色分离。
- 只连 PostgreSQL / MySQL / SQLite;其余数据源需经 DI 的 connectors。
