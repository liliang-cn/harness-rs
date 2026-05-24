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
};
