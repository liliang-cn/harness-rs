import { useState } from "react"

export type Kind = "postgres" | "mysql" | "sqlite"

export type Conn = {
  kind: Kind
  host: string
  port: number
  user: string
  password: string
  database: string
  ssl: boolean
}

const DEFAULT_PORT: Record<Kind, number> = { postgres: 5432, mysql: 3306, sqlite: 0 }
const ENGINES: { id: Kind; label: string; hint: string }[] = [
  { id: "postgres", label: "PostgreSQL", hint: "自建数仓、新一代系统" },
  { id: "mysql", label: "MySQL / MariaDB", hint: "多数 ERP、POS、进销存" },
  { id: "sqlite", label: "SQLite 文件", hint: "单机软件导出的 .db 文件" },
]

const EMPTY: Conn = {
  kind: "postgres", host: "", port: 5432, user: "", password: "", database: "", ssl: false,
}

/**
 * 一次性的连接设置(给 IT / 实施做,不是给老板做)。
 * 分字段填写:没有人应该被要求手写 `postgres://user:pass@host/db` 这种东西。
 * SQLite 是文件而不是服务器,选它时表单只留文件路径——不给用户看无意义的主机/账号。
 */
export default function Setup({
  api,
  initial,
  onDone,
  onCancel,
}: {
  api: (p: string, i?: RequestInit) => Promise<Response>
  initial?: Partial<Conn>
  onDone: () => void
  onCancel?: () => void
}) {
  const [c, setC] = useState<Conn>({ ...EMPTY, ...initial })
  const [busy, setBusy] = useState(false)
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null)
  const isFile = c.kind === "sqlite"

  const set = (k: keyof Conn) => (e: React.ChangeEvent<HTMLInputElement>) =>
    setC(v => ({
      ...v,
      [k]: k === "port" ? Number(e.target.value) || 0 : k === "ssl" ? e.target.checked : e.target.value,
    }))

  const pickEngine = (kind: Kind) => setC(v => ({ ...v, kind, port: DEFAULT_PORT[kind] }))

  const call = async (path: string, okText: (d: any) => string) => {
    setBusy(true); setMsg(null)
    try {
      const d = await (await api(path, {
        method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(c),
      })).json()
      setMsg({ ok: !!d.ok, text: d.ok ? okText(d) : `连接失败:${d.error}` })
      return !!d.ok
    } catch (e: any) {
      setMsg({ ok: false, text: e.message }); return false
    } finally { setBusy(false) }
  }

  // 文件库只要路径;服务器库要主机/账号/库名
  const canSubmit = isFile ? !!c.database : !!(c.host && c.user && c.database)

  return (
    <div className="setup">
      <h2>连接你的数据库</h2>
      <p className="hint">
        这一步通常由公司的 IT 或 ERP 供应商完成,只需做一次。
        建议使用<strong>只读账号</strong>——本产品也只会执行查询。
      </p>

      <div className="engines">
        {ENGINES.map(e => (
          <button key={e.id} type="button"
            className={"engine" + (c.kind === e.id ? " on" : "")}
            onClick={() => pickEngine(e.id)}>
            <div className="engine-name">{e.label}</div>
            <div className="engine-hint">{e.hint}</div>
          </button>
        ))}
      </div>

      <div className="grid">
        {isFile ? (
          <label className="wide">
            数据库文件路径
            <input value={c.database} onChange={set("database")} placeholder="/Users/you/data/shop.db" />
            <span className="sub">把 .db / .sqlite 文件拖到访达里看路径,或直接粘贴</span>
          </label>
        ) : (
          <>
            <label>主机 / IP<input value={c.host} onChange={set("host")} placeholder="192.168.1.50" /></label>
            <label className="narrow">端口<input value={c.port} onChange={set("port")} inputMode="numeric" /></label>
            <label>账号<input value={c.user} onChange={set("user")} placeholder="readonly" /></label>
            <label>密码<input value={c.password} onChange={set("password")} type="password" /></label>
            <label>数据库名<input value={c.database} onChange={set("database")} placeholder="erp_prod" /></label>
            <label className="check">
              <input type="checkbox" checked={c.ssl} onChange={set("ssl")} />
              <span>使用 SSL 加密(内网库通常不需要)</span>
            </label>
          </>
        )}
      </div>

      {msg && <div className={"setup-msg " + (msg.ok ? "ok" : "err")}>{msg.text}</div>}

      <div className="setup-actions">
        <button className="ghost" type="button" disabled={busy || !canSubmit}
          onClick={() => call("/setup/test", d => `✓ 连接成功(${d.engine}),发现 ${d.tables} 张表 — ${d.summary}`)}>
          测试连接
        </button>
        <button className="send" type="button" disabled={busy || !canSubmit}
          onClick={async () => { if (await call("/setup/save", () => "已保存")) onDone() }}>
          {busy ? "处理中…" : "保存并开始使用"}
        </button>
        {onCancel && <button className="ghost" type="button" onClick={onCancel}>返回</button>}
      </div>
    </div>
  )
}
