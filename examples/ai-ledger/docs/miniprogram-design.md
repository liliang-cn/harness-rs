# ai-ledger 微信小程序 · 开发设计文档

目标：把现有的 ai-ledger Web 端（`http://43.167.167.6:6743/`）的核心能力搬到微信小程序，复用现成的 Rust HTTP API，零后端改动（或极小改动）。

---

## 1. API 现状总览

后端在 `examples/ai-ledger/src/server.rs` + `src/admin.rs`，全部走 `Authorization: Bearer <token>`（除注册/登录）。下面按用途分组。

### 1.1 公开 / 鉴权入口

| Method | Path | 用途 | 备注 |
|---|---|---|---|
| GET  | `/api/info`     | 模型清单 + 默认 model | 不需 token |
| POST | `/api/register` | 邮箱密码注册（可带 `invite_code`） | trial / paid / admin 三档 tier |
| POST | `/api/login`    | 邮箱密码登录 → 返回 `{token, user}` | 小程序保存 token 到 `wx.setStorage` |
| POST | `/api/logout`   | 登出当前 session | token 失效后再调 `/api/login` |

### 1.2 当前用户 (`/api/me/*`)

| Method | Path | 用途 |
|---|---|---|
| GET    | `/api/me`             | `{user, effective_model_id}` |
| GET    | `/api/me/invites`     | 我创建的活跃邀请码（已用完的自动过滤掉） |
| POST   | `/api/me/invites`     | 生成新邀请码 |
| POST   | `/api/me/password`    | 改密码（同时踢掉其他 session） |
| POST   | `/api/me/model`       | 切换偏好模型（paid+ 才能用） |
| GET    | `/api/me/memories`    | 我的长期记忆列表 |
| DELETE | `/api/me/memories`    | 清空全部记忆 |
| DELETE | `/api/me/memories/:id`| 删单条记忆 |

### 1.3 记账（ledger 域）

| Method | Path | 用途 |
|---|---|---|
| GET  | `/api/accounts`      | 我的账户列表 |
| GET  | `/api/transactions`  | 流水（支持 `month=YYYY-MM`） |
| GET  | `/api/report`        | 月度报表：总额 + 分类汇总 |
| GET  | `/api/budgets`       | 预算 vs 当月支出 |
| GET  | `/api/subscriptions` | 周期扣款列表 + `月均` |
| POST | `/api/subscriptions/:id/cancel` | 取消订阅 |
| POST | `/api/brief`         | 让 LLM 生成本月小结（结构化 JSON） |

### 1.4 投资（portfolio 域）

| Method | Path | 用途 |
|---|---|---|
| GET  | `/api/portfolio/assets`     | 持有的资产清单 + 最新价 |
| GET  | `/api/portfolio/trades`     | 交易历史（仅 buy/sell） |
| GET  | `/api/portfolio/positions`  | 持仓（含浮盈） |
| GET  | `/api/portfolio/summary`    | 组合汇总（按币种） |
| POST | `/api/portfolio/refresh-prices` | 主动刷新行情（Yahoo→Tencent→Gemini fallback） |

### 1.5 聊天（agent 域）⭐ 关键

| Method | Path | 用途 | 协议 |
|---|---|---|---|
| POST | `/api/chat`         | 老的一发一收（不带 session） | application/json |
| POST | `/api/chat/stream`  | 老的流式（不带 session） | **SSE** |
| GET  | `/api/chat/sessions`               | 我所有的会话 | JSON |
| POST | `/api/chat/sessions`               | 新建一个会话 | JSON |
| GET  | `/api/chat/sessions/:id`           | 单个会话 + 全部消息 | JSON |
| DELETE | `/api/chat/sessions/:id`         | 删会话（级联删消息） | — |
| POST | `/api/chat/sessions/:id/stream`    | 在会话里发一条消息 → 流式吐 | **SSE** |

### 1.6 数据导出（个人中心）

| Method | Path | 内容 |
|---|---|---|
| GET | `/api/me/export/transactions.csv`  | 流水：id / 类型 / 金额 / 币种 / 账户名 / 对方账户名 / 分类 / 备注 / 发生时间 / 创建时间 |
| GET | `/api/me/export/trades.csv`        | 投资交易：id / 标的 / 标的名称 / 类型（买入/卖出/建仓基线）/ 数量 / 单价 / 币种 / 手续费 / 金额合计 / 交易时间 / 备注 / 创建时间 |
| GET | `/api/me/export/subscriptions.csv` | 订阅（含已取消）：id / 名称 / 金额 / 币种 / 频率 / 下次扣款 / 扣款账户名 / 分类 / 支付渠道 / 备注 / 状态 / 创建时间 / 取消时间 |

每次导出后端写一条 `audit_events.kind = "export"`，可在 admin 审计页追踪。响应头：

```
Content-Type:        text/csv; charset=utf-8
Content-Disposition: attachment; filename="transactions-YYYYMMDD.csv"
Cache-Control:       no-store
```

Body 开头带 UTF-8 BOM (`﻿`)，Excel / Numbers / Sheets 直接打开 CN 字符不乱码；逗号 / 双引号 / 换行字段按 RFC 4180 escape；account_id / asset_id 已 join 成账户名 / 标的名，不暴露 raw id。

### 1.7 Admin（仅 `tier == "admin"`）

| Method | Path | 用途 |
|---|---|---|
| GET    | `/api/admin/users`                       | 全部用户 + 聚合指标 |
| GET    | `/api/admin/users/:id`                   | 单个用户详情 |
| PATCH  | `/api/admin/users/:id`                   | 改 tier |
| DELETE | `/api/admin/users/:id`                   | 级联删除用户 |
| POST   | `/api/admin/users/:id/reset-password`    | 生成临时密码 |
| GET    | `/api/admin/audit?user_id=&kind=&before_ms=&limit=` | 审计日志（分页） |
| GET    | `/api/admin/logs?lines=200`              | systemd journalctl tail |
| GET    | `/api/admin/config`                      | provider 配置（key 已脱敏） |
| PATCH  | `/api/admin/config`                      | 改 key / 默认模型（写 DB + 热生效） |

小程序版本不实现 admin（管理端继续走 Web）。

---

## 2. 范围与不做的事

| ✅ 做 | ❌ 不做 |
|---|---|
| 登录 / 注册（邮箱 + 邀请码） | Admin 端（继续用 Web `/admin`） |
| 记账核心：账户 / 流水 / 预算 / 月报 / 订阅 | 月度简报（`/api/brief` 输出富文本 + 投资段，小屏阅读差） |
| 投资：持仓、汇总、刷新行情 | Memory 管理（高级功能，先放后面） |
| AI 聊天（**长按说话** + 流式吐字） | 模型切换（默认 tier 决定） |
| 我的：邀请码（生成 + 复制 + 分享） | 改密码（先用 Web 改） |
| 数据导出 CSV（流水 / 交易 / 订阅） | xlsx / pdf 报表 |

---

## 3. 技术栈选型

| 维度 | 选 | 不选 | 理由 |
|---|---|---|---|
| 框架 | **原生小程序 (WXML/WXSS/JS)** | Taro / Uni-app | 后端已经是固定的 REST + SSE，跨端价值低；原生包体小、调试链路短 |
| 状态 | `Page.data` + 单例 `app.globalData` | MobX / Pinia | 7-8 个页面，单层 state 足够 |
| UI | 微信原生组件 + **少量 Vant Weapp**（Toast / Dialog / Tab / Cell） | colorUI / TDesign | Vant 包体最小、文档稳定，足够覆盖列表/表单 |
| 请求 | 封装 `wx.request` + token 拦截 + 401 跳登录 | `taro-runtime` 等 | 200 行就够 |
| 流式 | 见 §6 | — | SSE 在小程序里不通，需要 workaround |
| 存储 | `wx.setStorageSync('token', …)` + `wx.setStorageSync('me', …)` | — | 跟 Web 端的 localStorage 一一对应 |

**包体目标**：主包 < 1.5 MB，分包（投资、AI）< 2 MB / 个，整体 < 8 MB（微信硬上限 16 MB）。

---

## 4. 后端需要做的改动

**HTTPS 是硬要求** —— 微信小程序生产环境必须 HTTPS 且备案域名。建议：

1. 把当前 qc-jp 节点套上 nginx + Let's Encrypt（推荐 Caddy 更简单，自动签证）
2. 给小程序后台加上 request 合法域名：`https://ledger.<your-domain>`
3. 开发期可在小程序开发者工具里勾选「不校验合法域名」临时用 HTTP，但发布前必须切

CORS 头已经在后端开了（`tower_http::cors::Any`），小程序请求不会被卡。

可选改动：

- **SSE → JSON 长轮询代替（见 §6）**：可以新增 `POST /api/chat/sessions/:id/poll` 一次返回完整 reply，省去客户端流式拼接。
- **登录返回字段**：现在 `/api/login` 返 `{token, user}` 直接够用；不用改。

---

## 5. 页面结构

```
app.json
  pages/
    login/           # 邮箱密码 + 邀请码
    home/            # 今日支出 / 上次流水 / 进入聊天的入口
    ledger/
      index          # 账户卡 + 月度汇总 + 流水表
      add            # 手动加流水（备用，主要靠 AI）
      budgets        # 预算
      subscriptions  # 订阅列表
    portfolio/
      index          # 组合汇总
      positions      # 持仓表
      trades         # 交易历史
    chat/
      list           # 会话列表
      session        # 单会话（消息流 + 输入框 + 长按说话）
    me/
      index          # 邀请码 / 导出数据 / 退出登录 / 记忆入口
      memories       # AI 记得我什么（list + 删除）
      invites        # 邀请码列表 + 复制 + 转发好友
      export         # 导出数据（3 个按钮触发 wx.downloadFile，见 §7.5）
  utils/
    api.js           # request 封装、token、401 拦截
    fmt.js           # 金额 / 日期格式化
  components/
    money-cell/      # ¥1,234.56 + 涨跌色
    chat-bubble/     # user / asst 气泡 + markdown 简易渲染
```

底部 tabBar：**记账 · 投资 · 聊天 · 我的**（4 个），跟 Web 端的 mode-toggle 概念对应。

---

## 6. SSE 在小程序里不通 —— workaround

`wx.request` 没有事件流；`wx.connectSocket` 是 WebSocket。后端的 `/api/chat/sessions/:id/stream` 是 axum 的 SSE，体积小但小程序无法直接订阅。

三个选项：

| 方案 | 难度 | 体验 | 推荐 |
|---|---|---|---|
| **A. 后端加一个 chunked-text 端点**：`POST .../stream-chunked` 用 `Transfer-Encoding: chunked + text/plain`，小程序用 `wx.request` 的 `responseType: 'arraybuffer'` + `enableChunked: true`（基础库 ≥ 2.20.0）逐 chunk 拼。 | 中 | ★★★★ 流式逐字出 | ⭐ |
| **B. 加一个一次性 `/poll` 端点**：服务器同步等 agent 跑完，一次性返完整 reply。客户端 loading 转圈 5–30 秒。 | 低 | ★★ 等几秒一下出完 | 备选 |
| C. 走 WebSocket：后端改成 `axum::extract::ws`，客户端 `wx.connectSocket`。 | 高 | ★★★★ | 不值 |

**推荐 A**。落地手顺：

1. 后端：新增 `POST /api/chat/sessions/:id/stream-chunked` —— 内部跟 SSE 一样跑 agent，但把 chunks 用 `text/plain` 直接写 body（每行一个 JSON，类似 NDJSON）。复用现有的 `Outcome::Done` 拼装逻辑。
2. 小程序：

```js
// utils/api.js
function streamChat(sessionId, message, onChunk) {
  const req = wx.request({
    url: `${BASE}/api/chat/sessions/${sessionId}/stream-chunked`,
    method: 'POST',
    enableChunked: true,
    responseType: 'arraybuffer',
    header: { Authorization: 'Bearer ' + getToken() },
    data: { message },
  });
  req.onChunkReceived((res) => {
    const txt = new TextDecoder().decode(new Uint8Array(res.data));
    txt.split('\n').filter(Boolean).forEach((line) => {
      try { onChunk(JSON.parse(line)); } catch {}
    });
  });
  return req; // 调用方可以 req.abort()
}
```

如果不想动后端，先上方案 B，等其他都跑通再回头做 A。

---

## 7. 关键交互对照

| Web 端行为 | 小程序对应 |
|---|---|
| 顶部 tabs `📊 记账 / 📈 投资 / 👤 我的` | 底部 tabBar 4 项 |
| 浮动 chat FAB → 弹窗模态 | 「聊天」作为 tabBar 主入口 |
| 聊天里长按 🎤 说话 → Web Speech API | `wx.startRecord` → audio file → 调腾讯 WeixinJSBridge speechRecognizer **或** 走后端 STT（Gemini 音频接口） |
| 邀请链接复制：`http://.../?invite=XXX` | 转发给微信好友：分享卡片 `path=/pages/login/index?invite=XXX`；落地页 `onLoad(query)` 自动填邀请码 |
| markdown 渲染（marked.js） | 用 `towxml` 组件，或简易自己拼 `<rich-text>` |
| 记忆系统、`@audit log` | 记忆放在「我的 → AI 记得我什么」；audit 不暴露 |
| 我的 → 导出数据 → 三个按钮 → `fetch + Authorization` → blob → `<a download>` | 见 §7.5（小程序没有 blob URL，要走 `wx.downloadFile` + `wx.openDocument`） |

**长按说话的小程序版**（关键差异）：

- Web 端走的是浏览器内置 Web Speech；小程序里 `wx.startRecord` 只能录音，不能直接转文字。
- 方案 1（最稳）：录音 → upload 到后端 → 新端点 `POST /api/voice/transcribe` 调 Gemini audio `inlineData` 转文字 → 返给客户端塞入输入框。
- 方案 2（绕过去）：调微信 [insertSpeechRecognizer](https://developers.weixin.qq.com/community/develop/article/doc/0008e6b6fbcf08aa17a4ba3925bc13) 即时识别（基础库 ≥ 2.13.0，仅安卓 + iOS 14.5+）。

推荐先做方案 1，跟现有 Gemini key 复用。

### 7.5 数据导出在小程序里的落地

小程序没有 `Blob` / `URL.createObjectURL` / `<a download>`。流程改成：

1. `wx.downloadFile` 拉文件到临时路径，**关键：自定义 header 传 token**
2. 拉完拿到 `tempFilePath`
3. 用 `wx.openDocument` 让微信内置文档查看器打开 CSV（Excel / Numbers 那种），或 `wx.shareFileMessage` 直接转发给微信好友 / 文件传输助手

```js
// pages/me/index.js
function exportCsv(kind /* 'transactions' | 'trades' | 'subscriptions' */) {
  wx.showLoading({ title: '导出中…' });
  wx.downloadFile({
    url: `${BASE}/api/me/export/${kind}.csv`,
    header: { Authorization: 'Bearer ' + getApp().globalData.token },
    success(res) {
      wx.hideLoading();
      if (res.statusCode !== 200) {
        wx.showToast({ title: 'HTTP ' + res.statusCode, icon: 'none' });
        return;
      }
      // 选一：内置文档查看器（CSV 可读但不能编辑）
      wx.openDocument({
        filePath: res.tempFilePath,
        fileType: 'csv',
        showMenu: true, // 右上角分享菜单，方便发到「文件传输助手」
      });
      // 或者直接弹分享：
      // wx.shareFileMessage({ filePath: res.tempFilePath });
    },
    fail(err) {
      wx.hideLoading();
      wx.showToast({ title: '导出失败', icon: 'none' });
      console.error(err);
    },
  });
}
```

注意点：

- `wx.downloadFile` 域名必须配进「downloadFile 合法域名」白名单（不是 request 那一栏）；开发期可暂时勾选「不校验合法域名」。
- 临时文件存活期 ≤ 24h；要长期保留，调 `wx.saveFile` 转到永久存储（占用户的小程序文件配额）。
- 文件名由后端 `Content-Disposition` 决定，但小程序的 `wx.downloadFile` 不暴露这个 header；想给文件带友好名字，转给文件传输助手时手动 `wx.shareFileMessage({ fileName: 'transactions-20260522.csv', filePath })`。
- 小程序内置查看器对 CSV 体验一般（不能选列、不能横向滚动太流畅）。推荐用法是「导出 → 立刻转发到电脑微信 → 电脑用 Excel 打开」。

---

## 8. 认证 & 邀请落地

```
┌── Step 1：未登录直接打开小程序 ──┐
│   - 登录页：邮箱 + 密码 + 可选邀请码  │
│   - 如果 path query 里有 invite，   │
│     自动填入 + 切到注册 tab          │
│   - POST /api/login or /api/register│
│   - wx.setStorageSync('token', t)   │
└──────────────────────────────────────┘
│
├── Step 2：每次冷启动 / 切前台
│     - app.onLaunch / onShow 读 token
│     - 调 GET /api/me 校验
│     - 401 → 清 token → 跳登录页
│
└── Step 3：每个 wx.request 拦截器
      - 自动注入 Authorization
      - 401 → 同上
      - 5xx → toast 错误，不强制登出
```

邀请链接通过微信分享卡片传递 `?invite=` query。`pages/login/index.js`：

```js
onLoad(query) {
  if (query.invite) {
    this.setData({
      mode: 'register',
      inviteCode: query.invite,
    });
  }
}
```

---

## 9. 开发与发布节奏

| 阶段 | 工作 | 周期估算（业余强度） |
|---|---|---|
| Step 0 | qc-jp 节点上 HTTPS（Caddy） | 1 晚 |
| Step 1 | 小程序骨架 + 登录 + tabBar + 我的 | 1 周 |
| Step 2 | 记账三页（账户 / 流水 / 月报） | 1 周 |
| Step 3 | 聊天 list + session + 文字版收发（方案 B 用 `/poll`） | 1 周 |
| Step 4 | 流式：方案 A（后端 + 客户端 chunked） | 3 天 |
| Step 5 | 语音输入（录音 + 上传 + 后端调 Gemini） | 3 天 |
| Step 6 | 投资三页 | 4 天 |
| Step 7 | 订阅 / 邀请码 / 分享卡片 | 2 天 |
| Step 8 | 内测 → 提审 → 上架 | 1–2 周等审 |

并行进度：UI 截图 + APPID 申请 + ICP 备案可以从 Step 0 起就开干。

---

## 10. 已知坑点（提前打预防针）

1. **HTTPS + 备案**：没备案的域名小程序后台加不进 request 白名单。如果暂时拿不下来 ICP，先发到"内部体验版"，体验版可以绕开（但仍要 HTTPS）。
2. **基础库版本**：`enableChunked: true` 要 ≥ 2.20.0，`onChunkReceived` 要 ≥ 2.20.1。app.json 里强制 `requiredBackgroundModes` 不需要，但建议指定 `requiredPrivateInfos` (用到 mic 时)。
3. **markdown**：`<rich-text>` 不支持 script/link，AI 回复里可能带 markdown 表格、代码块——上 towxml 或回退到纯文本。
4. **金额精度**：现在所有 amount 字段是字符串（`rust_decimal::Decimal` 序列化），小程序里别误用 `Number()`，直接显示原字符串或用 `BigNumber.js` 做加减。
5. **时区**：后端 `HARNESS_USER_TZ=Asia/Shanghai`，月报按这个切月。客户端展示日期前后端保持一致即可。
6. **小程序不能写 cookie**：登出靠客户端清 token；后端 `/api/logout` 只是审计写一行，不依赖 cookie。

---

## 11. 下一步

确认两个事情就能开工：

- [ ] HTTPS / 备案域名要不要现在就上？还是先用「不校验合法域名」开发模式跑通整个流程？
- [ ] 流式聊天先方案 A 还是 B？(B 一晚搞定，A 更顺滑但要改后端)

代码层面建议放在新仓库：`ledger-miniapp/`（独立 git repo），跟 ai-ledger 主仓解耦——小程序的 IDE/CI/发布流程完全不同。
