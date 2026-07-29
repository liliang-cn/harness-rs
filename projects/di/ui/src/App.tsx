import { useCallback, useEffect, useMemo, useRef, useState } from "react"
import { AIRenderer } from "@ai-gui/react"
import { chart } from "@ai-gui/plugin-chart"
import { evidence, serializeEvidenceFence } from "@ai-gui/plugin-evidence"
import Setup, { type Conn } from "./Setup"

type Db = { name: string; size: string; governed?: boolean }
type Msg = { id: number; role: "user" | "ai"; text: string; status?: string; done?: boolean }
let nextId = 1

/** 令牌:首次可由 URL ?t= 带入并记住,之后所有请求都带上。 */
const TOKEN_KEY = "advisor_token"
function readToken(): string {
  const fromUrl = new URLSearchParams(location.search).get("t")
  if (fromUrl) {
    localStorage.setItem(TOKEN_KEY, fromUrl)
    history.replaceState(null, "", location.pathname)   // 别把令牌留在地址栏
    return fromUrl
  }
  return localStorage.getItem(TOKEN_KEY) ?? ""
}
let token = readToken()
const authHeaders = (extra: Record<string, string> = {}) => ({
  ...extra,
  ...(token ? { Authorization: `Bearer ${token}` } : {}),
})
/** 带令牌的 fetch;401 时提示需要令牌。 */
async function api(path: string, init: RequestInit = {}) {
  const res = await fetch(path, { ...init, headers: authHeaders(init.headers as any) })
  if (res.status === 401) throw new Error("未授权:请用启动时打印的链接打开(含访问令牌)")
  return res
}

const SUGGESTIONS = [
  "这个库是做什么生意的?先给我一页纸的经营概览",
  "哪些门店/品类在亏损?根因是什么?给排序的整改建议",
  "最近两个完整月的营收环比如何?哪里变化最大?",
  "成本结构拆解:各项费用占营收的比例",
]

export default function App() {
  const [dbs, setDbs] = useState<Db[]>([])
  const [current, setCurrent] = useState("")
  const [model, setModel] = useState("")
  const [msgs, setMsgs] = useState<Msg[]>([])
  const [input, setInput] = useState("")
  const [busy, setBusy] = useState(false)
  const [authErr, setAuthErr] = useState("")
  const [needSetup, setNeedSetup] = useState<null | boolean>(null)
  const [savedConn, setSavedConn] = useState<Partial<Conn> | undefined>()
  const [governed, setGoverned] = useState(false)
  const [genBusy, setGenBusy] = useState(false)
  const bottomRef = useRef<HTMLDivElement>(null)

  // AIGUI 插件:图表 + 代码高亮。theme 交给渲染器,图表才不会在深色页上出白底。
  const plugins = useMemo(
    () => [chart({ interactive: true }), evidence({ title: "数据来源" })],
    [],
  )

  useEffect(() => {
    // 先看有没有配过库:没配就先进设置页
    api("/setup").then(r => r.json()).then(d => {
      setNeedSetup(!d.configured)
      setSavedConn(d.saved ?? undefined)
      if (d.configured) {
        api("/databases").then(r => r.json()).then(x => {
          setDbs(x.databases ?? []); setCurrent(x.current ?? ""); setGoverned(!!x.governed)
        }).catch(() => {})
      }
    }).catch(e => setAuthErr(String(e.message)))
    api("/model").then(r => r.json()).then(d => setModel(d.model ?? "")).catch(() => {})
  }, [])

  useEffect(() => { bottomRef.current?.scrollIntoView({ behavior: "smooth" }) }, [msgs])

  const switchDb = useCallback(async (name: string) => {
    const r = await api("/use", {
      method: "POST", headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ db: name }),
    }).then(x => x.json())
    if (r.ok) {
      setCurrent(name)
      setGoverned(!!r.governed)   // 以服务端返回为准,避免闭包里的过期状态
      setMsgs([{ id: nextId++, role: "ai", text: `已切换到数据库 **${name}**,可以开始提问。`, done: true }])
    } else {
      setMsgs(m => [...m, { id: nextId++, role: "ai", text: `切换失败:${r.error}`, done: true }])
    }
  }, [])

  const genModel = useCallback(async () => {
    setGenBusy(true)
    try {
      const d = await (await api("/model/generate", { method: "POST" })).json()
      setMsgs(m => [...m, {
        id: nextId++, role: "ai", done: true,
        text: d.ok
          ? `已为 **${d.database}** 生成语义模型(${d.metrics} 个指标),该库切换到**治理模式**:查数走固定口径,跨 grain 自动编译正确,并带权限与脱敏。生成的模型建议由懂业务的人再校一遍。`
          : `生成失败:${d.error}`,
      }])
      if (d.ok) {
        setGoverned(true)
        setDbs(list => list.map(x => x.name === d.database ? { ...x, governed: true } : x))
      }
    } catch (e: any) {
      setMsgs(m => [...m, { id: nextId++, role: "ai", text: `生成失败:${e.message}`, done: true }])
    } finally { setGenBusy(false) }
  }, [])

  const ask = useCallback(async (question?: string) => {
    const message = (question ?? input).trim()
    if (!message || busy) return
    setInput(""); setBusy(true)
    const aiId = nextId + 1
    setMsgs(m => [...m,
      { id: nextId++, role: "user", text: message },
      { id: nextId++, role: "ai", text: "", status: "正在分析…" }])
    const t0 = performance.now()
    const askedAt = Date.now()
    // 按 id 定位这条回答,避免并发/异步下用下标错位
    const patch = (p: Partial<Msg>) =>
      setMsgs(m => m.map(x => (x.id === aiId ? { ...x, ...p } : x)))

    try {
      const res = await api("/chat/stream", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ session_id: `web-${Date.now()}`, message }),
      })
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const reader = res.body!.getReader()
      const dec = new TextDecoder()
      let buf = "", acc = "", sawDone = false
      for (;;) {
        const { done, value } = await reader.read()
        if (done) break
        buf += dec.decode(value, { stream: true })
        let nl: number
        while ((nl = buf.indexOf("\n")) !== -1) {
          const line = buf.slice(0, nl).trim(); buf = buf.slice(nl + 1)
          if (!line.startsWith("data:")) continue
          const payload = line.slice(5).trim()
          if (!payload) continue
          let f: any
          try { f = JSON.parse(payload) } catch { continue }
          const secs = ((performance.now() - t0) / 1000).toFixed(1)
          if (f.type === "step") patch({ status: `⋯ ${f.label} · ${secs}s` })
          else if (f.type === "token") { acc += f.text; patch({ text: acc }) }
          else if (f.type === "done") {
            acc = f.answer ?? acc; sawDone = true
            patch({ text: acc, status: `✔ 完成 · ${secs}s · 已查库 · 审计留痕`, done: true })
            // 溯源由服务端提供:模型能编数字,就能编"证据",所以证据只认真实执行过的查询
            void appendEvidence(askedAt, () => acc, patch)
          }
        }
      }
      // 流正常结束但没收到 done 帧:内容已渲染,标记完成即可,不报错
      if (!sawDone) {
        const secs = ((performance.now() - t0) / 1000).toFixed(1)
        patch({ status: `✔ 完成 · ${secs}s`, done: true })
      }
    } catch (e: any) {
      patch({ status: `✖ ${e.message}`, done: true })
    } finally { setBusy(false) }
  }, [input, busy])

  return (
    <div className="app">
      <header>
        <div className="logo">AI</div>
        <div>
          <h1>AI 战略顾问</h1>
          <div className="sub">直连你的数据库 · 只读 · 数字全部来自查询,不编造</div>
        </div>
        <div className="conn">
          <span className="dot" />
          <select value={current} onChange={e => switchDb(e.target.value)}>
            {dbs.map(d => <option key={d.name} value={d.name}>{d.governed ? "🛡 " : ""}{d.name} · {d.size}</option>)}
          </select>
          <span className={"mode " + (governed ? "gov" : "raw")}
                title={governed ? "已配语义模型:按固定口径查数,跨 grain 正确,含权限与脱敏" : "无语义模型:模型自己写只读 SQL"}>
            {governed ? "🛡 治理模式" : "直连模式"}
          </span>
          {!governed && (
            <button className="genbtn" disabled={genBusy} onClick={genModel}>
              {genBusy ? "生成中…" : "生成语义模型"}
            </button>
          )}
          {model && <span className="model">{model}</span>}
        </div>
      </header>

      <main>
        {authErr && <div className="auth-err">{authErr}</div>}
        {!authErr && needSetup === true && (
          <Setup api={api} initial={savedConn} onDone={() => { setNeedSetup(false); location.reload() }} />
        )}
        {msgs.length === 0 && !authErr && needSetup === false && (
          <div className="chips">
            {SUGGESTIONS.map(s => (
              <button key={s} className="chip" onClick={() => ask(s)}>{s}</button>
            ))}
          </div>
        )}
        {msgs.map((m, i) =>
          m.role === "user" ? (
            <div key={i} className="msg user"><div className="bubble">{m.text}</div></div>
          ) : (
            <div key={i} className="msg ai">
              <div className="bubble">
                {m.status && <div className={"status" + (m.done ? " done" : "")}>{m.status}</div>}
                {/* 受控 text:每次传完整文本,渲染器自己算增量并整体重解析,图表不闪 */}
                <AIRenderer text={m.text} plugins={plugins} theme="light" />
              </div>
            </div>
          )
        )}
        <div ref={bottomRef} />
      </main>

      {needSetup === false && <div className="composer">
        <div className="wrap">
          <textarea
            value={input}
            placeholder="问一个经营/战略问题,顾问会直接查你的库来回答…"
            onChange={e => setInput(e.target.value)}
            onKeyDown={e => { if ((e.metaKey || e.ctrlKey) && e.key === "Enter") ask() }}
          />
          <button className="send" disabled={busy || !input.trim()} onClick={() => ask()}>
            {busy ? "分析中…" : "分析"}
          </button>
        </div>
        <div className="foot">只读连接 · 仅允许 SELECT · 每次查询 hash 链审计留痕</div>
      </div>}
    </div>
  )
}

/** 取本轮真实执行过的查询,作为 evidence 块附到回答末尾(服务端事实,模型无法伪造)。 */
async function appendEvidence(
  askedAt: number,
  getText: () => string,
  patch: (p: { text: string }) => void,
) {
  try {
    const d = await (await api(`/evidence?since=${askedAt}`)).json()
    const queries = (d.queries ?? []).filter((q: any) => q?.query)
    if (queries.length === 0) return
    patch({ text: `${getText()}\n\n${serializeEvidenceFence({ queries })}` })
  } catch { /* 拿不到证据就不显示,不影响答案 */ }
}
