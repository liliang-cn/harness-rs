// Tiny fetch wrapper. Token lives in localStorage; 401s clear it.
const TOKEN_KEY = 'ai-ledger-admin-token';

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
    // Match BrowserRouter basename: dev runs at "/", prod under "/admin".
    const loginUrl = import.meta.env.PROD ? '/admin/login' : '/login';
    window.location.href = loginUrl;
    throw new ApiError(401, 'unauthorized');
  }
  if (!resp.ok) {
    let msg = `${resp.status}`;
    try {
      const j = await resp.json();
      msg = j.error || j.message || msg;
    } catch {
      // swallow body parse error
    }
    throw new ApiError(resp.status, msg);
  }
  if (resp.status === 204) return undefined as unknown as T;
  return (await resp.json()) as T;
}

// ─── login (uses main app's /api/login) ───
export async function login(email: string, password: string) {
  const j = await api<{ token: string; user: { id: string; email: string; tier: string } }>(
    '/api/login',
    {
      method: 'POST',
      body: JSON.stringify({ email, password }),
    },
  );
  setToken(j.token);
  return j.user;
}

export interface UserStats {
  id: string;
  email: string;
  tier: string;
  created_at: string;
  txn_count: number;
  chat_count: number;
  last_seen_at: string | null;
  tokens_in: number;
  tokens_out: number;
  cost_usd?: number;
  invited_by: string | null;
  invite_code_used: string | null;
}

export interface AuditEvent {
  id: string;
  user_id: string | null;
  kind: string;
  target_id: string | null;
  meta_json: string | null;
  tokens_in: number;
  tokens_out: number;
  created_ms: number;
}

export interface ProviderConfigView {
  deepseek_key_masked: string;
  gemini_key_masked: string;
  default_model_id: string;
  available_models: {
    id: string;
    label: string;
    provider: string;
    available: boolean;
  }[];
}

export interface Invite {
  code: string;
  created_by: string;
  uses_remaining: number;
  expires_at: string | null;
  created_at: string;
}

export const adminApi = {
  listInvites: () => api<{ invites: Invite[] }>('/api/me/invites'),
  createInvite: () =>
    api<{ invite: Invite }>('/api/me/invites', { method: 'POST' }),
  listUsers: () =>
    api<{ users: UserStats[]; priced_at_model?: string }>('/api/admin/users'),
  getUser: (id: string) =>
    api<{
      user: UserStats & { preferred_model: string | null; trade_count: number };
      recent_audit: AuditEvent[];
    }>(`/api/admin/users/${id}`),
  patchUser: (id: string, body: { tier?: string }) =>
    api<{ ok: true; tier: string }>(`/api/admin/users/${id}`, {
      method: 'PATCH',
      body: JSON.stringify(body),
    }),
  resetPassword: (id: string) =>
    api<{ ok: true; temp_password: string }>(
      `/api/admin/users/${id}/reset-password`,
      { method: 'POST' },
    ),
  deleteUser: (id: string) =>
    api<{ ok: true }>(`/api/admin/users/${id}`, { method: 'DELETE' }),
  listAudit: (params: { user_id?: string; kind?: string; before_ms?: number; limit?: number }) => {
    const q = new URLSearchParams();
    for (const [k, v] of Object.entries(params)) {
      if (v !== undefined && v !== '') q.set(k, String(v));
    }
    return api<{ events: AuditEvent[]; next_before_ms: number | null }>(
      `/api/admin/audit?${q.toString()}`,
    );
  },
  getLogs: (lines = 200) =>
    api<{ lines: string; error?: string }>(`/api/admin/logs?lines=${lines}`),
  getConfig: () => api<ProviderConfigView>('/api/admin/config'),
  patchConfig: (body: {
    deepseek_api_key?: string;
    gemini_api_key?: string;
    default_model_id?: string;
  }) =>
    api<{ ok: true; changed: string[] }>('/api/admin/config', {
      method: 'PATCH',
      body: JSON.stringify(body),
    }),
};
