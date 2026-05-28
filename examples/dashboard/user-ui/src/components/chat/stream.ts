// SSE consumer for /api/chat/sessions/:id/stream.
//
// The Rust handler emits one JSON object per `data:` line, with these shapes
// (see ChannelHook + session_stream_handler in src/server.rs):
//
//   {"type":"start"}
//   {"type":"iter","iter":N}
//   {"type":"token","text":"..."}                 ← live token deltas
//   {"type":"thought","text":"..."}               ← non-streaming model text
//   {"type":"tool_start","name":"...","args":...}
//   {"type":"tool_end","name":"...","ok":bool,"preview":...}
//   {"type":"error","message":"..."}
//   {"type":"done","ok":bool,"iters":N,"reply":"...","warning":?}
//
// We map these to a smaller `StreamEvent` union the UI consumes. Tool calls
// surface as `tool_start`/`tool_end` for inline status lines; `iter` is
// dropped (purely diagnostic). `thought` is treated like a delta so the
// non-streaming code path still appears live.
import { getToken } from '@/lib/api';
import { asArtifactSpec } from '@/lib/artifact';

export type StreamEvent =
  | { type: 'start' }
  | { type: 'delta'; text: string }
  | { type: 'tool_start'; name: string }
  | { type: 'tool_end'; name: string; ok: boolean }
  | { type: 'done'; ok: boolean; reply: string; warning?: string }
  | { type: 'error'; message: string }
  | { type: 'artifact'; spec: import('@/lib/artifact').ArtifactSpec };

export async function streamSession(
  sessionId: string,
  message: string,
  onEvent: (e: StreamEvent) => void,
  signal?: AbortSignal,
  lang?: string,
  attachment_ids?: string[],
): Promise<void> {
  let resp: Response;
  try {
    resp = await fetch(`/api/chat/sessions/${sessionId}/stream`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        Authorization: `Bearer ${getToken() ?? ''}`,
      },
      body: JSON.stringify({ message, lang, attachment_ids }),
      signal,
    });
  } catch (e) {
    onEvent({ type: 'error', message: (e as Error).message || 'network error' });
    onEvent({ type: 'done', ok: false, reply: '' });
    return;
  }

  if (!resp.ok || !resp.body) {
    let msg = `HTTP ${resp.status}`;
    try {
      const j = await resp.json();
      msg = j.error || j.message || msg;
    } catch {
      /* keep status */
    }
    onEvent({ type: 'error', message: msg });
    onEvent({ type: 'done', ok: false, reply: '' });
    return;
  }

  const reader = resp.body.getReader();
  const dec = new TextDecoder();
  let buf = '';
  let sawDone = false;

  while (true) {
    let chunk: ReadableStreamReadResult<Uint8Array>;
    try {
      chunk = await reader.read();
    } catch (e) {
      onEvent({ type: 'error', message: (e as Error).message || 'stream aborted' });
      break;
    }
    if (chunk.done) break;
    buf += dec.decode(chunk.value, { stream: true });

    let nl: number;
    while ((nl = buf.indexOf('\n\n')) !== -1) {
      const evt = buf.slice(0, nl);
      buf = buf.slice(nl + 2);
      for (const line of evt.split('\n')) {
        if (!line.startsWith('data:')) continue;
        const json = line.slice(5).trimStart();
        if (!json) continue;
        let obj: Record<string, unknown>;
        try {
          obj = JSON.parse(json) as Record<string, unknown>;
        } catch {
          continue;
        }
        const mapped = mapEvent(obj);
        if (!mapped) continue;
        if (mapped.type === 'done') sawDone = true;
        onEvent(mapped);
      }
    }
  }

  if (!sawDone) onEvent({ type: 'done', ok: true, reply: '' });
}

function mapEvent(obj: Record<string, unknown>): StreamEvent | null {
  const t = obj['type'];
  switch (t) {
    case 'start':
      return { type: 'start' };
    case 'token':
    case 'thought': {
      const text = typeof obj['text'] === 'string' ? (obj['text'] as string) : '';
      if (!text) return null;
      return { type: 'delta', text };
    }
    case 'tool_start': {
      const name = typeof obj['name'] === 'string' ? (obj['name'] as string) : 'tool';
      return { type: 'tool_start', name };
    }
    case 'tool_end': {
      const name = typeof obj['name'] === 'string' ? (obj['name'] as string) : 'tool';
      const ok = obj['ok'] !== false;
      return { type: 'tool_end', name, ok };
    }
    case 'error': {
      const message =
        typeof obj['message'] === 'string'
          ? (obj['message'] as string)
          : 'agent error';
      return { type: 'error', message };
    }
    case 'done': {
      const ok = obj['ok'] !== false;
      const reply = typeof obj['reply'] === 'string' ? (obj['reply'] as string) : '';
      const warning =
        typeof obj['warning'] === 'string' ? (obj['warning'] as string) : undefined;
      return { type: 'done', ok, reply, warning };
    }
    case 'artifact': {
      const spec = asArtifactSpec(obj);
      return spec ? { type: 'artifact', spec } : null;
    }
    case 'iter':
    default:
      return null;
  }
}
