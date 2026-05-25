const TOKEN_KEY = 'ai-note-token';
export function getToken(): string | null { return localStorage.getItem(TOKEN_KEY); }
export function setToken(t: string | null) {
  if (t) localStorage.setItem(TOKEN_KEY, t); else localStorage.removeItem(TOKEN_KEY);
}

export type Space = 'work' | 'life';

export interface Note {
  id: string; title: string; body: string; tags: string[];
  space: Space; created_at: string; updated_at: string;
}
export interface SearchHit extends Note { score: number; via_grep: boolean; }
export interface ChatSession {
  id: string; title: string; space: Space; model_id?: string;
  message_count: number; created_at: string; updated_at: string;
}
export interface ChatMessage {
  id: string; session_id: string; role: string; text: string;
  created_at: string; truncated?: boolean;
}

async function req<T>(path: string, init?: RequestInit): Promise<T> {
  const resp = await fetch(path, {
    ...init,
    headers: {
      'Content-Type': 'application/json',
      Authorization: `Bearer ${getToken() ?? ''}`,
      ...(init?.headers ?? {}),
    },
  });
  if (!resp.ok) {
    let msg = `HTTP ${resp.status}`;
    try { const j = await resp.json(); msg = j.error || j.message || msg; } catch { /* */ }
    throw new Error(msg);
  }
  return resp.json() as Promise<T>;
}

export const noteApi = {
  login: (email: string, password: string) =>
    req<{ token: string; user: any }>('/api/login', { method: 'POST', body: JSON.stringify({ email, password }) }),
  register: (email: string, password: string, invite_code?: string) =>
    req<{ token: string; user: any }>('/api/register', { method: 'POST', body: JSON.stringify({ email, password, invite_code }) }),
  me: () => req<{ user: any }>('/api/me'),
  info: () => req<{ model: string; allowed_models: string[] }>('/api/info'),
  changePassword: (old_password: string, new_password: string) =>
    req<{ ok: boolean }>('/api/me/password', { method: 'POST', body: JSON.stringify({ old_password, new_password }) }),
  setModel: (model: string | null) =>
    req<{ ok: boolean; model: string | null }>('/api/me/model', { method: 'POST', body: JSON.stringify({ model }) }),

  notes: (space: Space) => req<{ notes: Note[] }>(`/api/notes?space=${space}&limit=200`),
  createNote: (space: Space, title: string, body: string, tags: string[]) =>
    req<{ note: Note }>('/api/notes', { method: 'POST', body: JSON.stringify({ space, title, body, tags }) }),
  updateNote: (id: string, patch: Partial<Pick<Note, 'title' | 'body' | 'tags'>>) =>
    req<{ ok: boolean }>(`/api/notes/${id}`, { method: 'PATCH', body: JSON.stringify(patch) }),
  deleteNote: (id: string) => req<{ ok: boolean }>(`/api/notes/${id}`, { method: 'DELETE' }),
  search: (space: Space, q: string) =>
    req<{ hits: SearchHit[] }>(`/api/notes/search?space=${space}&q=${encodeURIComponent(q)}&limit=20`),

  chatSessions: (space: Space) => req<{ sessions: ChatSession[] }>(`/api/chat/sessions?space=${space}`),
  createChatSession: (space: Space) =>
    req<{ session: ChatSession }>('/api/chat/sessions', { method: 'POST', body: JSON.stringify({ space }) }),
  getChatSession: (id: string) =>
    req<{ session: ChatSession; messages: ChatMessage[] }>(`/api/chat/sessions/${id}`),
  deleteChatSession: (id: string) =>
    req<{ deleted: string }>(`/api/chat/sessions/${id}`, { method: 'DELETE' }),
};
