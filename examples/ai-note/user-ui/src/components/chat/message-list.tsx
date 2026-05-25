import { useEffect, useRef } from 'react';
import { useTranslation } from 'react-i18next';
import { Loader2, AlertCircle, RotateCw } from 'lucide-react';
import type { ChatMessage } from '@/lib/api';
import { renderMarkdown } from '@/lib/markdown';
import { cn } from '@/lib/utils';
import { Button } from '@/components/ui/button';

/** Inline status events surfaced under the streaming bubble. */
export type ToolEvent =
  | { kind: 'tool_start'; id: number; name: string }
  | { kind: 'tool_end'; id: number; name: string; ok: boolean }
  | { kind: 'error'; id: number; message: string };

interface MessageListProps {
  messages: ChatMessage[];
  streaming: string | null;
  toolEvents: ToolEvent[];
  busy: boolean;
  onReload?: () => void;
}

export function MessageList({
  messages,
  streaming,
  toolEvents,
  busy,
  onReload,
}: MessageListProps) {
  const { t } = useTranslation();
  const bottomRef = useRef<HTMLDivElement | null>(null);

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
          <Bubble
            key={m.id}
            role={m.role}
            text={m.text}
            truncated={m.truncated}
            onReload={onReload}
          />
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
      </div>
      <div ref={bottomRef} />
    </div>
  );
}

function Bubble({
  role,
  text,
  streaming,
  truncated,
  onReload,
}: {
  role: string;
  text: string;
  streaming?: boolean;
  truncated?: boolean;
  onReload?: () => void;
}) {
  const { t } = useTranslation();
  const isUser = role === 'user';
  if (isUser) {
    return (
      <div className="flex flex-col items-end gap-1.5">
        {text.trim().length > 0 && (
          <div className="bg-primary text-primary-foreground max-w-[85%] rounded-2xl rounded-br-sm px-3 py-2 text-sm whitespace-pre-wrap break-words">
            {text}
          </div>
        )}
      </div>
    );
  }
  return (
    <div className="flex max-w-[90%] flex-col items-start gap-1">
      <div
        className={cn(
          'bg-muted text-foreground markdown-body rounded-2xl rounded-bl-sm px-3 py-2 text-sm break-words',
        )}
      >
        <div
          dangerouslySetInnerHTML={{
            __html: renderMarkdown(text || (streaming ? '' : '')),
          }}
        />
        {streaming && <span className="animate-pulse">▍</span>}
      </div>
      {truncated && !streaming && (
        <div className="text-muted-foreground flex items-center gap-2 pl-1 text-xs">
          <AlertCircle className="text-amber-600 dark:text-amber-400 size-3.5" />
          <span>{t('chat.truncated', { defaultValue: 'Reply interrupted — partial.' })}</span>
          {onReload && (
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="h-6 px-2 text-xs"
              onClick={onReload}
            >
              <RotateCw className="size-3" />
              {t('chat.reload', { defaultValue: 'Reload' })}
            </Button>
          )}
        </div>
      )}
    </div>
  );
}
