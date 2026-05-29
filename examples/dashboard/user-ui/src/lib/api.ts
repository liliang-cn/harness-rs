// Thin fetch wrapper for the user-facing UI. Token in localStorage, 401
// clears it and bounces to /login.

const TOKEN_KEY = 'ledger-user-token';

export function getToken(): string | null {
  return localStorage.getItem(TOKEN_KEY);
}
export function setToken(t: string | null) {
  if (t) localStorage.setItem(TOKEN_KEY, t);
  else localStorage.removeItem(TOKEN_KEY);
}

class ApiError extends Error {
  status: number;
  constructor(status: number, msg: string) {
    super(msg);
    this.status = status;
  }
}

export async function api<T = unknown>(
  path: string,
  init: RequestInit = {},
): Promise<T> {
  const headers = new Headers(init.headers);
  const t = getToken();
  if (t) headers.set('Authorization', `Bearer ${t}`);
  if (!headers.has('Content-Type') && init.body) {
    headers.set('Content-Type', 'application/json');
  }
  const resp = await fetch(path, { ...init, headers });
  if (resp.status === 401) {
    setToken(null);
    if (!location.pathname.endsWith('/login')) location.href = '/login';
    throw new ApiError(401, 'unauthorized');
  }
  if (!resp.ok) {
    let msg = `${resp.status}`;
    try {
      const j = await resp.json();
      msg = j.error || j.message || msg;
    } catch {
      // body parse error — keep the status code
    }
    throw new ApiError(resp.status, msg);
  }
  if (resp.status === 204) return undefined as unknown as T;
  return (await resp.json()) as T;
}

// ─── domain types ─────────────────────────────────────────

export interface User {
  id: string;
  email: string;
  tier: string;
  base_currency: string;
  preferred_model?: string | null;
  created_at?: string;
  invited_by?: string | null;
  invite_code_used?: string | null;
}

// /api/info shape — only the fields user-ui needs right now.
export interface ModelOption {
  id: string;
  label: string;
  provider: string;
  available: boolean;
}

export interface ServerInfo {
  provider: string;
  model: string;
  default_model_id: string;
  available_models: ModelOption[];
}

// Backend uses harness-core MemoryEntry (memory.rs): content / source /
// created_ms (ms since epoch) / optional tags / optional expires_ms.
export interface MemoryEntry {
  id: string;
  content: string;
  tags?: string[];
  source?: string | null;
  created_ms: number;
  expires_ms?: number | null;
}

export interface NetWorthSnapshot {
  snapshot_date: string;
  base_currency: string;
  cash_amt: number;
  investments_amt: number;
  debt_amt: number;
  net_amt: number;
}

export interface Account {
  id: string;
  name: string;
  kind: 'cash' | 'debit' | 'credit' | 'wallet' | 'other';
  currency: string;
  opening_balance: string;
  created_at: string;
}

export interface Transaction {
  id: string;
  kind: 'expense' | 'income' | 'transfer';
  amount: string; // decimal as string for fidelity
  currency: string;
  account_id: string;
  counter_account_id: string | null;
  category: string | null;
  note: string | null;
  occurred_at: string; // RFC3339
  created_at: string;
}

export interface TxnQuery {
  from?: string; // ISO date — currently ignored server-side (client filter)
  to?: string;
  category?: string;
  account_id?: string;
  limit?: number;
}

// Server returns BudgetStatus rows (computed from Budget + this-month spend).
export interface BudgetStatus {
  category: string;
  currency: string;
  limit: string; // decimal-as-string
  used: string;
  remaining: string;
  over_budget: boolean;
}

export interface Loan {
  account_id: string;
  name: string;
  kind: 'loan' | 'mortgage' | 'receivable';
  counterparty: string;
  principal: string;
  remaining: string; // 2-decimal string
  currency: string;
  apr: string; // decimal as string, "0.045" = 4.5%
  term_months: number | null;
  monthly_payment: string | null;
  start_date: string;
  next_due_date: string | null;
  progress_pct: number; // 0..100, 2-decimal
  status: 'active' | 'paid_off';
  note: string | null;
}

export type SubscriptionFrequency = 'weekly' | 'monthly' | 'quarterly' | 'yearly';

export interface Subscription {
  id: string;
  name: string;
  amount: string;
  currency: string;
  frequency: SubscriptionFrequency;
  next_charge_date: string; // YYYY-MM-DD
  account_id: string;
  category: string | null;
  pay_channel: string | null;
  note: string | null;
  status: string; // "active" | "cancelled"
  created_at: string;
  cancelled_at: string | null;
}

// Row in /api/report by_category (server-side `CategoryTotal`).
export interface ReportRow {
  category: string;
  currency: string;
  total: string;
  count: number;
}

export type CsvExportKind = 'transactions' | 'trades' | 'subscriptions';

export type AssetClass = 'stock' | 'etf' | 'commodity' | 'crypto' | 'other';
export type TradeKind = 'buy' | 'sell' | 'opening';

export interface Asset {
  id: string;
  symbol: string;
  name: string;
  asset_class: AssetClass;
  provider_id: string | null;
  currency: string;
  created_at: string;
}

export interface PriceQuote {
  asset_id: string;
  price: string;
  currency: string;
  fetched_at: string;
  source: string;
}

// /api/portfolio/assets returns enriched rows: `{ asset, latest_price }`.
export interface AssetWithPrice {
  asset: Asset;
  latest_price: PriceQuote | null;
}

export interface Position {
  asset_id: string;
  symbol: string;
  name: string;
  asset_class: AssetClass;
  currency: string;
  qty: string;
  avg_cost: string;
  realized_pl: string;
  last_price: string | null;
  last_price_at: string | null;
  last_price_source: string | null;
  market_value: string | null;
  unrealized_pl: string | null;
}

export interface Trade {
  id: string;
  asset_id: string;
  kind: TradeKind;
  qty: string;
  price_per_unit: string;
  currency: string;
  fees: string;
  occurred_at: string;
  note: string | null;
  created_at: string;
}

// ─── projects + notes ─────────────────────────────────────

export type ProjectStatus = 'active' | 'paused' | 'done' | 'dropped';

export interface Project {
  id: string;
  name: string;
  detail: string;
  status: ProjectStatus;
  parent_id?: string | null;
  target_date?: string | null;
  review_interval_days?: number | null;
  next_review_at?: string | null;
  created_at: string;
  updated_at: string;
}

export interface ProjectReview {
  id: string;
  project_id: string;
  progress: string;
  next_steps: string;
  created_at: string;
}

export interface Note {
  id: string;
  project_id?: string | null;
  title: string;
  body: string;
  tags: string[];
  created_at: string;
  updated_at: string;
}

export interface NoteSearchHit extends Note {
  score: number;
  via_grep: boolean;
}

// ─── digest / notifications ───────────────────────────────

export interface DigestSettings {
  enabled: boolean;
  send_time: string; // "HH:MM"
  timezone: string;  // IANA
  channel: 'in_app' | 'email' | 'both';
  last_digest_date?: string | null;
}

export interface NotificationItem {
  id: string;
  kind: string;
  title: string;
  body: any;
  created_at: number;
  read_at: number | null;
}

// ─── chat ─────────────────────────────────────────────────

export interface Attachment {
  id: string;
  mime_type: string;
  size_bytes: number;
  kind: 'image' | 'pdf';
}

export async function uploadAttachment(file: File): Promise<Attachment> {
  const fd = new FormData();
  fd.append('file', file);
  const resp = await fetch('/api/chat/attachments', {
    method: 'POST',
    headers: { Authorization: `Bearer ${getToken() ?? ''}` },
    body: fd,
  });
  if (!resp.ok) {
    const text = await resp.text().catch(() => `HTTP ${resp.status}`);
    throw new Error(text);
  }
  return resp.json();
}

export function attachmentUrl(id: string): string {
  return `/api/chat/attachments/${encodeURIComponent(id)}`;
}

/** Bearer-protected blob fetch — use this for <img src> previews since
 *  the GET endpoint requires the Authorization header. Caller is
 *  responsible for URL.revokeObjectURL on unmount. */
export async function fetchAttachmentBlob(id: string): Promise<string> {
  const resp = await fetch(attachmentUrl(id), {
    headers: { Authorization: `Bearer ${getToken() ?? ''}` },
  });
  if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
  return URL.createObjectURL(await resp.blob());
}

export interface ChatSession {
  id: string;
  title: string | null;
  model_id: string | null;
  message_count: number;
  created_at: string;
  updated_at: string;
}

export interface ChatMessage {
  id: string;
  session_id: string;
  /** "user" | "asst" — server uses "asst" not "assistant" */
  role: 'user' | 'asst' | string;
  text: string;
  iters?: number;
  created_at: string;
  /** Attachment ids the user uploaded with this message. Empty for asst
   *  turns and for messages predating the feature. */
  attachment_ids?: string[];
  /** Client-side only flag. Set when an assistant message was committed
   *  from a stream that was aborted (sheet closed mid-reply). The UI
   *  shows a ⚠ marker + a reload button. Cleared once the canonical
   *  reply is fetched from the server. */
  truncated?: boolean;
  /** Artifacts (render_artifact specs) the assistant emitted this turn.
   *  Hydrated from the server on reload; appended live during streaming. */
  artifacts?: import('@/lib/artifact').ArtifactSpec[];
}

// ─── endpoints ────────────────────────────────────────────

export const ledgerApi = {
  login: (email: string, password: string) =>
    api<{ token: string; user: User }>('/api/login', {
      method: 'POST',
      body: JSON.stringify({ email, password }),
    }),
  register: (email: string, password: string, invite_code?: string) =>
    api<{ token: string; user: User }>('/api/register', {
      method: 'POST',
      body: JSON.stringify({ email, password, invite_code }),
    }),
  me: () =>
    api<{ user: User; effective_model_id: string }>('/api/me'),
  info: () => api<ServerInfo>('/api/info'),
  changePassword: (old_password: string, new_password: string) =>
    api<{ ok: true; other_sessions_dropped: number }>('/api/me/password', {
      method: 'POST',
      body: JSON.stringify({ old_password, new_password }),
    }),
  setModel: (model: string | null) =>
    api<{ preferred_model: string | null; effective_model_id: string }>(
      '/api/me/model',
      { method: 'POST', body: JSON.stringify({ model }) },
    ),
  memories: () =>
    api<{ count: number; memories: MemoryEntry[] }>('/api/me/memories'),
  deleteMemory: (id: string) =>
    api<{ deleted: string }>(
      `/api/me/memories/${encodeURIComponent(id)}`,
      { method: 'DELETE' },
    ),
  clearMemories: () =>
    api<{ deleted: number }>('/api/me/memories', { method: 'DELETE' }),
  netWorth: () => api<{ snapshot: NetWorthSnapshot }>('/api/me/net-worth'),
  netWorthSeries: (from?: string, to?: string) => {
    const q = new URLSearchParams();
    if (from) q.set('from', from);
    if (to) q.set('to', to);
    const qs = q.toString() ? `?${q}` : '';
    return api<{
      from: string;
      to: string;
      count: number;
      series: NetWorthSnapshot[];
    }>(`/api/me/net-worth/series${qs}`);
  },
  netWorthRefresh: () =>
    api<{ snapshot: NetWorthSnapshot }>('/api/me/net-worth/refresh', {
      method: 'POST',
    }),
  setBaseCurrency: (currency: string) =>
    api<{ ok: true; base_currency: string; snapshot: NetWorthSnapshot }>(
      '/api/me/base-currency',
      { method: 'POST', body: JSON.stringify({ currency }) },
    ),
  accounts: () => api<{ count: number; accounts: Account[] }>('/api/accounts'),
  transactions: (q: TxnQuery = {}) => {
    const p = new URLSearchParams();
    for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== '') p.set(k, String(v));
    const qs = p.toString() ? `?${p}` : '';
    return api<{ total_matched?: number; returned?: number; count?: number; transactions: Transaction[] }>(
      `/api/transactions${qs}`,
    );
  },
  budgets: (year?: number, month?: number) => {
    const p = new URLSearchParams();
    if (year !== undefined) p.set('year', String(year));
    if (month !== undefined) p.set('month', String(month));
    const qs = p.toString() ? `?${p}` : '';
    return api<{
      year: number;
      month: number;
      budgets: BudgetStatus[];
      over_count: number;
    }>(`/api/budgets${qs}`);
  },
  subscriptions: () =>
    api<{
      count: number;
      subscriptions: Subscription[];
      monthly_burn_by_currency: Record<string, string>;
    }>('/api/subscriptions'),
  cancelSubscription: (id: string) =>
    api<{ cancelled: string }>(`/api/subscriptions/${encodeURIComponent(id)}/cancel`, {
      method: 'POST',
    }),
  loans: () => api<{ loans: Loan[] }>('/api/me/loans'),
  positions: () =>
    api<{ count: number; positions: Position[] }>('/api/portfolio/positions'),
  trades: (asset_symbol?: string, limit?: number) => {
    const p = new URLSearchParams();
    if (asset_symbol) p.set('asset_symbol', asset_symbol);
    if (limit !== undefined) p.set('limit', String(limit));
    const qs = p.toString() ? `?${p}` : '';
    return api<{ count: number; trades: Trade[] }>(`/api/portfolio/trades${qs}`);
  },
  assets: () =>
    api<{ count: number; assets: AssetWithPrice[] }>('/api/portfolio/assets'),
  allocation: () =>
    api<{
      base_currency: string;
      total: number;
      by_class: { class: string; value: number; pct: number }[];
      missing_rate_for: string[];
    }>('/api/portfolio/allocation'),
  summary: () => api<Record<string, unknown>>('/api/portfolio/summary'),
  monthlyReport: (year?: number, month?: number) => {
    const p = new URLSearchParams();
    if (year !== undefined) p.set('year', String(year));
    if (month !== undefined) p.set('month', String(month));
    const qs = p.toString() ? `?${p}` : '';
    return api<{
      year: number;
      month: number;
      by_category: ReportRow[];
      grand_total_by_currency: Record<string, string>;
    }>(`/api/report${qs}`);
  },
  chatSessions: () =>
    api<{ count: number; sessions: ChatSession[] }>('/api/chat/sessions'),
  createChatSession: () =>
    api<{ session: ChatSession }>('/api/chat/sessions', { method: 'POST' }),
  getChatSession: (id: string) =>
    api<{ session: ChatSession; messages: ChatMessage[] }>(
      `/api/chat/sessions/${encodeURIComponent(id)}`,
    ),
  deleteChatSession: (id: string) =>
    api<{ deleted: string }>(`/api/chat/sessions/${encodeURIComponent(id)}`, {
      method: 'DELETE',
    }),
  // ─── projects ─────────────────────────────────────────────
  projects: (filter: 'active' | 'due' | 'all' = 'active') =>
    api<{ projects: Project[]; due_count: number }>(`/api/projects?filter=${filter}`),
  project: (id: string) =>
    api<{ project: Project; milestones: Project[]; reviews: ProjectReview[] }>(
      `/api/projects/${encodeURIComponent(id)}`,
    ),
  createProject: (body: {
    name: string;
    detail?: string;
    parent_id?: string;
    target_date?: string;
    review_interval_days?: number;
  }) =>
    api<{ project: Project }>('/api/projects', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  updateProject: (
    id: string,
    patch: Partial<Pick<Project, 'name' | 'detail' | 'status' | 'target_date' | 'review_interval_days'>>,
  ) =>
    api<{ ok: boolean }>(`/api/projects/${encodeURIComponent(id)}`, {
      method: 'PATCH',
      body: JSON.stringify(patch),
    }),
  deleteProject: (id: string) =>
    api<{ deleted: string }>(`/api/projects/${encodeURIComponent(id)}`, {
      method: 'DELETE',
    }),
  addProjectReview: (
    id: string,
    body: { progress: string; next_steps?: string; next_review_in_days?: number },
  ) =>
    api<{ review: ProjectReview }>(`/api/projects/${encodeURIComponent(id)}/reviews`, {
      method: 'POST',
      body: JSON.stringify(body),
    }),

  // ─── notes ────────────────────────────────────────────────
  notes: (projectId?: string) => {
    const q = projectId ? `?project_id=${encodeURIComponent(projectId)}` : '';
    return api<{ notes: Note[] }>(`/api/notes${q}`);
  },
  note: (id: string) =>
    api<{ note: Note }>(`/api/notes/${encodeURIComponent(id)}`),
  createNote: (body: {
    title?: string;
    body: string;
    tags?: string[];
    project_id?: string;
  }) =>
    api<{ note: Note }>('/api/notes', {
      method: 'POST',
      body: JSON.stringify(body),
    }),
  updateNote: (
    id: string,
    patch: Partial<Pick<Note, 'title' | 'body' | 'tags'>>,
  ) =>
    api<{ ok: boolean }>(`/api/notes/${encodeURIComponent(id)}`, {
      method: 'PATCH',
      body: JSON.stringify(patch),
    }),
  deleteNote: (id: string) =>
    api<{ deleted: string }>(`/api/notes/${encodeURIComponent(id)}`, {
      method: 'DELETE',
    }),
  searchNotes: (q: string, projectId?: string) => {
    const p = new URLSearchParams({ q });
    if (projectId) p.set('project_id', projectId);
    return api<{ hits: NoteSearchHit[] }>(`/api/notes/search?${p}`);
  },

  digestSettings: () =>
    api<{ settings: DigestSettings }>('/api/me/digest-settings'),
  saveDigestSettings: (s: { enabled: boolean; time: string; timezone: string; channel: string }) =>
    api<{ ok: boolean; settings: DigestSettings }>('/api/me/digest-settings', {
      method: 'PATCH',
      body: JSON.stringify(s),
    }),
  notifications: (unread = false) =>
    api<{ notifications: NotificationItem[]; unread: number }>(
      `/api/me/notifications${unread ? '?unread=true' : ''}`,
    ),
  markNotificationsRead: (ids?: string[]) =>
    api<{ ok: boolean; updated: number }>('/api/me/notifications/read', {
      method: 'POST',
      body: JSON.stringify(ids ? { ids } : {}),
    }),

  /**
   * CSV download is a special-case: the server returns text/csv with a
   * Content-Disposition filename, not JSON. We do an authenticated fetch,
   * blob() the body, and trigger a synthetic <a download> click.
   */
  exportCsv: async (kind: CsvExportKind): Promise<void> => {
    const headers = new Headers();
    const tok = getToken();
    if (tok) headers.set('Authorization', `Bearer ${tok}`);
    const r = await fetch(`/api/me/export/${kind}.csv`, { headers });
    if (r.status === 401) {
      setToken(null);
      if (!location.pathname.endsWith('/login')) location.href = '/login';
      throw new ApiError(401, 'unauthorized');
    }
    if (!r.ok) throw new ApiError(r.status, `HTTP ${r.status}`);
    const cd = r.headers.get('content-disposition') || '';
    const m = cd.match(/filename="?([^";]+)"?/i);
    const filename = m
      ? m[1]
      : `${kind}-${new Date().toISOString().slice(0, 10).replace(/-/g, '')}.csv`;
    const blob = await r.blob();
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = filename;
    document.body.appendChild(a);
    a.click();
    a.remove();
    // Give the browser a tick before revoking — some old Safari needed it.
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  },
};
