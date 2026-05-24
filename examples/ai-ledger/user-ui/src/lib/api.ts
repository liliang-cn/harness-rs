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
  me: () => api<{ user: User }>('/api/me'),
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
