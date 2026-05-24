import { useEffect, useRef } from 'react';
import { useTranslation } from 'react-i18next';
import { Loader2, Wrench, AlertCircle } from 'lucide-react';
import type { ChatMessage } from '@/lib/api';
import { renderMarkdown } from '@/lib/markdown';
import { cn } from '@/lib/utils';

/** Inline status events surfaced under the streaming bubble. */
export type ToolEvent =
  | { kind: 'tool_start'; id: number; name: string }
  | { kind: 'tool_end'; id: number; name: string; ok: boolean }
  | { kind: 'error'; id: number; message: string };

interface MessageListProps {
  messages: ChatMessage[];
  /** Live assistant text being streamed (rendered as an in-progress bubble). */
  streaming: string | null;
  /** Tool/status timeline for the current stream. */
  toolEvents: ToolEvent[];
  /** True when the request is in-flight (shows thinking spinner if no text yet). */
  busy: boolean;
}

export function MessageList({
  messages,
  streaming,
  toolEvents,
  busy,
}: MessageListProps) {
  const { t } = useTranslation();
  const bottomRef = useRef<HTMLDivElement | null>(null);

  // Auto-scroll to bottom on any change. `auto` (not smooth) so the latest
  // delta never lags behind the cursor.
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ behavior: 'auto', block: 'end' });
  }, [messages, streaming, toolEvents.length, busy]);

  const isEmpty = messages.length === 0 && streaming === null && !busy;

  return (
    <div className="flex-1 overflow-y-auto px-3 py-4">
      {isEmpty && (
        <div className="text-muted-foreground py-12 text-center text-sm">
          {t('chat.emptyMessages')}
        </div>
      )}
      <div className="space-y-3">
        {messages.map((m) => (
          <Bubble key={m.id} role={m.role} text={m.text} />
        ))}
        {streaming !== null && (
          <Bubble role="asst" text={streaming} streaming />
        )}
        {busy && streaming === null && (
          <div className="text-muted-foreground flex items-center gap-2 text-xs">
            <Loader2 className="size-3 animate-spin" />
            {t('chat.thinking')}
          </div>
        )}
        {toolEvents.length > 0 && (
          <div className="space-y-1 pt-1">
            {toolEvents.map((e) => (
              <ToolLine key={e.id} ev={e} />
            ))}
          </div>
        )}
      </div>
      <div ref={bottomRef} />
    </div>
  );
}

function Bubble({
  role,
  text,
  streaming,
}: {
  role: string;
  text: string;
  streaming?: boolean;
}) {
  const isUser = role === 'user';
  if (isUser) {
    return (
      <div className="flex justify-end">
        <div className="bg-primary text-primary-foreground max-w-[85%] rounded-2xl rounded-br-sm px-3 py-2 text-sm whitespace-pre-wrap break-words">
          {text}
        </div>
      </div>
    );
  }
  return (
    <div className="flex justify-start">
      <div
        className={cn(
          'bg-muted text-foreground markdown-body max-w-[90%] rounded-2xl rounded-bl-sm px-3 py-2 text-sm break-words',
        )}
      >
        <div
          dangerouslySetInnerHTML={{
            __html: renderMarkdown(text || (streaming ? '' : '')),
          }}
        />
        {streaming && <span className="animate-pulse">▍</span>}
      </div>
    </div>
  );
}

function ToolLine({ ev }: { ev: ToolEvent }) {
  const { t } = useTranslation();
  if (ev.kind === 'error') {
    return (
      <div className="text-destructive flex items-start gap-1.5 text-xs italic">
        <AlertCircle className="mt-0.5 size-3 shrink-0" />
        <span>{t('chat.error', { message: ev.message })}</span>
      </div>
    );
  }
  const isStart = ev.kind === 'tool_start';
  const key = isStart
    ? 'chat.toolCalling'
    : ev.ok
      ? 'chat.toolDone'
      : 'chat.toolFailed';
  return (
    <div className="text-muted-foreground flex items-center gap-1.5 text-xs italic">
      {isStart ? (
        <Loader2 className="size-3 animate-spin" />
      ) : (
        <Wrench className="size-3" />
      )}
      <span>{t(key, { name: ev.name })}</span>
    </div>
  );
}
