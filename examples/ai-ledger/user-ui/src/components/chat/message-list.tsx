import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Loader2, Wrench, AlertCircle, FileText } from 'lucide-react';
import type { ChatMessage } from '@/lib/api';
import { fetchAttachmentBlob } from '@/lib/api';
import { renderMarkdown } from '@/lib/markdown';
import { cn } from '@/lib/utils';
import { Dialog, DialogContent, DialogTitle } from '@/components/ui/dialog';

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
          <Bubble
            key={m.id}
            role={m.role}
            text={m.text}
            attachmentIds={m.attachment_ids}
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
  attachmentIds,
}: {
  role: string;
  text: string;
  streaming?: boolean;
  attachmentIds?: string[];
}) {
  const isUser = role === 'user';
  if (isUser) {
    const hasAttachments = (attachmentIds?.length ?? 0) > 0;
    const hasText = text.trim().length > 0;
    return (
      <div className="flex flex-col items-end gap-1.5">
        {hasAttachments && (
          <div className="flex flex-wrap justify-end gap-1.5">
            {attachmentIds!.map((id) => (
              <AttachmentThumb key={id} id={id} />
            ))}
          </div>
        )}
        {hasText && (
          <div className="bg-primary text-primary-foreground max-w-[85%] rounded-2xl rounded-br-sm px-3 py-2 text-sm whitespace-pre-wrap break-words">
            {text}
          </div>
        )}
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

/**
 * Square thumb for an attachment hung off a message bubble. Click → opens
 * a full-screen Dialog. Falls back to a 📄 icon for non-image kinds (we
 * don't know the kind from the message row — only the id — so probe via
 * the blob's MIME type after fetch).
 */
function AttachmentThumb({ id }: { id: string }) {
  const [url, setUrl] = useState<string | null>(null);
  const [mime, setMime] = useState<string>('');
  const [open, setOpen] = useState(false);

  useEffect(() => {
    let cancelled = false;
    let blobUrl: string | null = null;
    (async () => {
      try {
        const resp = await fetch(`/api/chat/attachments/${encodeURIComponent(id)}`, {
          headers: {
            Authorization: `Bearer ${localStorage.getItem('ledger-user-token') ?? ''}`,
          },
        });
        if (!resp.ok || cancelled) return;
        const blob = await resp.blob();
        if (cancelled) return;
        blobUrl = URL.createObjectURL(blob);
        setUrl(blobUrl);
        setMime(blob.type);
      } catch {
        /* leave placeholder */
      }
    })();
    return () => {
      cancelled = true;
      if (blobUrl) URL.revokeObjectURL(blobUrl);
    };
  }, [id]);

  // Re-derive a viewer URL for the dialog from the same id. We use the
  // already-fetched blob URL if available; otherwise the dialog fires its
  // own fetch via the hook below when it opens.
  const isImage = mime.startsWith('image/');

  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(true)}
        className="bg-muted hover:opacity-80 size-16 overflow-hidden rounded-lg"
        aria-label="attachment"
      >
        {isImage && url ? (
          <img src={url} alt="" className="size-full object-cover" />
        ) : (
          <div className="text-muted-foreground flex size-full items-center justify-center">
            <FileText className="size-5" />
          </div>
        )}
      </button>
      <AttachmentDialog
        id={id}
        open={open}
        onOpenChange={setOpen}
        cachedUrl={url}
        cachedMime={mime}
      />
    </>
  );
}

function AttachmentDialog({
  id,
  open,
  onOpenChange,
  cachedUrl,
  cachedMime,
}: {
  id: string;
  open: boolean;
  onOpenChange: (v: boolean) => void;
  cachedUrl: string | null;
  cachedMime: string;
}) {
  const [url, setUrl] = useState<string | null>(cachedUrl);
  const [mime, setMime] = useState<string>(cachedMime);
  // If thumb-fetch hasn't finished yet, fetch fresh when the dialog opens.
  useEffect(() => {
    if (!open || url) return;
    let cancelled = false;
    let blobUrl: string | null = null;
    fetchAttachmentBlob(id)
      .then((u) => {
        if (cancelled) {
          URL.revokeObjectURL(u);
          return;
        }
        blobUrl = u;
        setUrl(u);
        // Best-effort: peek the blob via fetch HEAD-equivalent — skip;
        // assume image when the cached MIME is unknown.
        setMime('image/*');
      })
      .catch(() => {});
    return () => {
      cancelled = true;
      if (blobUrl) URL.revokeObjectURL(blobUrl);
    };
  }, [open, id, url]);

  const isImage = (mime || cachedMime).startsWith('image/');

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        className="bg-background/95 max-h-[90vh] max-w-[90vw] overflow-hidden p-2"
        showCloseButton
      >
        <DialogTitle className="sr-only">attachment</DialogTitle>
        {isImage && url ? (
          <img
            src={url}
            alt=""
            className="mx-auto max-h-[85vh] max-w-full rounded-md object-contain"
          />
        ) : (
          <div className="text-muted-foreground flex h-64 items-center justify-center">
            <FileText className="size-12" />
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}
