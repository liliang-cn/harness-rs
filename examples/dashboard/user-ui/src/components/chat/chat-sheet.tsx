import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ArrowLeft, Plus, RotateCcw, X } from 'lucide-react';
import { toast } from 'sonner';
import {
  Sheet,
  SheetContent,
  SheetTitle,
  SheetDescription,
} from '@/components/ui/sheet';
import { Button } from '@/components/ui/button';
import {
  ledgerApi,
  type ChatMessage,
  type ChatSession,
} from '@/lib/api';
import { SessionsList } from './sessions-list';
import { MessageList, type ToolEvent } from './message-list';
import { Composer } from './composer';
import { streamSession } from './stream';
import { subscribeChatPrefill } from '@/lib/chat-prefill';

interface ChatSheetProps {
  open: boolean;
  onOpenChange: (v: boolean) => void;
}

/**
 * 3-zone Sheet body when a session is active:
 *   - header (back + session title + new-chat)
 *   - MessageList (scrollable, fills remaining height)
 *   - Composer (sticky bottom)
 * When no session is active, the SessionsList replaces those.
 *
 * The Sheet uses side="right" on desktop. We override the radix width with
 * `!w-full sm:!max-w-md` so on mobile it becomes a full-screen overlay,
 * which is closer to the native bottom-up feel without juggling two `side`
 * props.
 */
export function ChatSheet({ open, onOpenChange }: ChatSheetProps) {
  const { t, i18n } = useTranslation();
  const [activeId, setActiveId] = useState<string | null>(null);
  const [session, setSession] = useState<ChatSession | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [streaming, setStreaming] = useState<string | null>(null);
  const [toolEvents, setToolEvents] = useState<ToolEvent[]>([]);
  const [busy, setBusy] = useState(false);
  const [sessionsKey, setSessionsKey] = useState(0);
  // "Draft" — user clicked "+ New chat" but hasn't sent anything yet.
  // We hold off on creating the DB session until the first real message
  // arrives, so empty FAB-clicks don't leave 0-message rows behind.
  const [drafting, setDrafting] = useState(false);
  // Set by openChatWith() (other pages) to seed the composer. New object per
  // call so the Composer's effect re-fires even for identical text.
  const [prefill, setPrefill] = useState<{ text: string } | null>(null);
  const abortRef = useRef<AbortController | null>(null);
  const toolIdRef = useRef(0);
  // Set when handleSend just lazy-created the session — we already know
  // it's empty on the server and we're about to optimistically append a
  // user bubble. Skip the load-effect's GET to avoid racing it.
  const skipNextLoad = useRef(false);

  /** Fetch the canonical message log from the server. Used by the
   *  load-on-select effect, the reopen-refresh effect (Fix 1), and the
   *  truncated-bubble's Reload button (Fix 2). */
  const reloadMessages = useCallback(async (id: string) => {
    try {
      const j = await ledgerApi.getChatSession(id);
      setSession(j.session);
      setMessages(j.messages);
      // Mark all messages as "seen" for the unread badge (Fix 3).
      try {
        const raw = localStorage.getItem('chat-seen-count') ?? '{}';
        const seen = JSON.parse(raw) as Record<string, number>;
        seen[id] = j.session.message_count;
        localStorage.setItem('chat-seen-count', JSON.stringify(seen));
      } catch {
        /* swallow — localStorage is best-effort */
      }
    } catch (e) {
      toast.error((e as Error).message);
    }
  }, []);

  // Load messages whenever a session is selected.
  useEffect(() => {
    if (!activeId) {
      setSession(null);
      setMessages([]);
      setStreaming(null);
      setToolEvents([]);
      return;
    }
    if (skipNextLoad.current) {
      skipNextLoad.current = false;
      return;
    }
    let cancelled = false;
    ledgerApi
      .getChatSession(activeId)
      .then((j) => {
        if (cancelled) return;
        setSession(j.session);
        setMessages(j.messages);
        try {
          const raw = localStorage.getItem('chat-seen-count') ?? '{}';
          const seen = JSON.parse(raw) as Record<string, number>;
          seen[activeId] = j.session.message_count;
          localStorage.setItem('chat-seen-count', JSON.stringify(seen));
        } catch {
          /* ignore */
        }
      })
      .catch((e) => {
        if (!cancelled) toast.error((e as Error).message);
      });
    return () => {
      cancelled = true;
    };
  }, [activeId]);

  // If the sheet closes mid-stream, abort the fetch so we don't keep the
  // generator running in the background. Also reset draft state so the
  // next open lands on the sessions list (otherwise a drafted-but-never-
  // sent chat would silently re-open).
  useEffect(() => {
    if (!open) {
      abortRef.current?.abort();
      abortRef.current = null;
      setDrafting(false);
      setPrefill(null);
    }
  }, [open]);

  // Other pages call openChatWith(text) to pop the chat open with the
  // composer pre-filled (e.g. "Add project", "Review"). Subscribe here so
  // those clicks actually open the sheet and seed a fresh draft.
  useEffect(() => {
    return subscribeChatPrefill((text) => {
      setActiveId(null);
      setMessages([]);
      setStreaming(null);
      setToolEvents([]);
      setDrafting(true);
      setPrefill({ text });
      onOpenChange(true);
    });
  }, [onOpenChange]);

  // Fix 1: when the sheet re-opens while a session is already active,
  // refresh from the server. Covers the case where the user closed
  // mid-stream and the agent kept running on the backend — the canonical
  // assistant reply is now in the DB but our local messages still show
  // the truncated stream-* row.
  const wasOpenRef = useRef(open);
  useEffect(() => {
    if (open && !wasOpenRef.current && activeId && !busy) {
      reloadMessages(activeId);
    }
    wasOpenRef.current = open;
  }, [open, activeId, busy, reloadMessages]);

  // Enter draft mode — no API call yet. handleSend creates the session
  // lazily on first message.
  const handleNew = useCallback(() => {
    setActiveId(null);
    setSession(null);
    setMessages([]);
    setStreaming(null);
    setToolEvents([]);
    setDrafting(true);
  }, []);

  const handleBack = useCallback(() => {
    abortRef.current?.abort();
    abortRef.current = null;
    setActiveId(null);
    setDrafting(false);
    setBusy(false);
    setSessionsKey((k) => k + 1);
  }, []);

  const handleStop = useCallback(() => {
    abortRef.current?.abort();
    abortRef.current = null;
  }, []);

  const handleSend = useCallback(
    async (
      text: string,
      attachment_ids: string[] = [],
      opts: { regenerate?: boolean } = {},
    ) => {
      if (busy) return;
      // Lazy-create the DB session on the first send of a draft. Avoids
      // leaving 0-message orphans when the user opens chat, clicks "+",
      // and closes without typing.
      let sessionId = activeId;
      if (!sessionId) {
        try {
          const j = await ledgerApi.createChatSession();
          sessionId = j.session.id;
          skipNextLoad.current = true;
          setActiveId(sessionId);
          setDrafting(false);
        } catch (e) {
          toast.error((e as Error).message);
          return;
        }
      }
      // Append the user bubble only on a fresh send. Regenerate keeps the
      // existing user message (it was already shown last turn) and just
      // re-runs the agent against it.
      if (!opts.regenerate) {
        const optimistic: ChatMessage = {
          id: `local-${Date.now()}`,
          session_id: sessionId,
          role: 'user',
          text,
          created_at: new Date().toISOString(),
          attachment_ids,
        };
        setMessages((cur) => [...cur, optimistic]);
      }
      setStreaming('');
      setToolEvents([]);
      setBusy(true);
      const ctrl = new AbortController();
      abortRef.current = ctrl;
      let buf = '';
      let finalReply = '';
      let gotDone = false;

      await streamSession(
        sessionId,
        text,
        (ev) => {
          switch (ev.type) {
            case 'start':
              break;
            case 'delta':
              buf += ev.text;
              setStreaming(buf);
              break;
            case 'tool_start':
              setToolEvents((cur) => [
                ...cur,
                { kind: 'tool_start', id: ++toolIdRef.current, name: ev.name },
              ]);
              break;
            case 'tool_end':
              setToolEvents((cur) => [
                ...cur,
                {
                  kind: 'tool_end',
                  id: ++toolIdRef.current,
                  name: ev.name,
                  ok: ev.ok,
                },
              ]);
              break;
            case 'error':
              setToolEvents((cur) => [
                ...cur,
                { kind: 'error', id: ++toolIdRef.current, message: ev.message },
              ]);
              break;
            case 'done':
              gotDone = true;
              finalReply = ev.reply || buf;
              if (ev.warning === 'budget_exhausted') {
                toast.warning(t('chat.budgetExhausted'));
              }
              break;
          }
        },
        ctrl.signal,
        i18n.language,
        attachment_ids,
      );

      // Commit the assistant message. Use server-provided `reply` if non-empty
      // (it includes any post-processing); otherwise fall back to accumulated
      // deltas. ID prefix `stream-` keys this row distinctly until next refresh.
      // If the stream was aborted (sheet closed mid-reply), mark truncated
      // so the bubble shows a ⚠ + Reload affordance.
      const wasAborted = ctrl.signal.aborted;
      const replyText = finalReply || buf;
      if (replyText) {
        const assistant: ChatMessage = {
          id: `stream-${Date.now()}`,
          session_id: sessionId,
          role: 'asst',
          text: replyText,
          created_at: new Date().toISOString(),
          truncated: wasAborted || !gotDone,
        };
        setMessages((cur) => [...cur, assistant]);
      }
      setStreaming(null);
      setToolEvents([]);
      setBusy(false);
      abortRef.current = null;

      if (!gotDone) {
        // Stream aborted or errored without a final event — keep partial.
      }

      // Refresh sessions list (message_count + updated_at moved).
      setSessionsKey((k) => k + 1);
    },
    [activeId, busy, t, i18n.language],
  );

  /** Drop the trailing assistant turn (if any) and re-run the last user
   *  message. Shown only when there IS a last user message and we're idle. */
  const handleRegenerate = useCallback(() => {
    if (busy) return;
    // Walk from the end to find the last user message.
    let lastUserIdx = -1;
    for (let i = messages.length - 1; i >= 0; i--) {
      if (messages[i].role === 'user') {
        lastUserIdx = i;
        break;
      }
    }
    if (lastUserIdx < 0) return;
    const lastUserText = messages[lastUserIdx].text;
    // Drop everything after that user message (the assistant turn we
    // didn't like, plus any stray tool/error messages).
    setMessages((cur) => cur.slice(0, lastUserIdx + 1));
    // Re-run without appending another user bubble. Pass empty
    // attachment_ids — we can't re-extract a receipt mid-confirmation.
    handleSend(lastUserText, [], { regenerate: true });
  }, [busy, messages, handleSend]);

  // Show the regenerate affordance only when:
  //   - we have an active session (i.e. not on the sessions list)
  //   - there's at least one user message in the transcript
  //   - we're not currently streaming
  const canRegenerate =
    !!activeId &&
    !busy &&
    messages.some((m) => m.role === 'user');

  // Show the sessions picker only when we have no session AND aren't
  // composing a new one. Drafting means user clicked "+ New chat" — we
  // give them the empty chat view + composer immediately.
  const showSessions = !activeId && !drafting;

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent
        side="right"
        showCloseButton={false}
        className="flex w-full flex-col gap-0 p-0 sm:max-w-md"
      >
        <SheetTitle className="sr-only">{t('chat.title')}</SheetTitle>
        <SheetDescription className="sr-only">{t('chat.fab')}</SheetDescription>
        <div className="border-border flex h-14 items-center gap-1 border-b px-3">
          {!showSessions && (
            <Button
              variant="ghost"
              size="icon-sm"
              aria-label={t('chat.back')}
              onClick={handleBack}
            >
              <ArrowLeft className="size-4" />
            </Button>
          )}
          <div className="min-w-0 flex-1 truncate px-1 text-sm font-medium">
            {showSessions
              ? t('chat.title')
              : session?.title?.trim() || t('chat.untitled')}
          </div>
          {!showSessions && (
            <Button
              variant="ghost"
              size="icon-sm"
              aria-label={t('chat.newChat')}
              title={t('chat.newChat')}
              onClick={handleNew}
            >
              <Plus className="size-4" />
            </Button>
          )}
          <Button
            variant="ghost"
            size="icon-sm"
            aria-label={t('chat.close', { defaultValue: 'close' })}
            onClick={() => onOpenChange(false)}
          >
            <X className="size-4" />
          </Button>
        </div>
        {showSessions ? (
          <SessionsList
            onSelect={setActiveId}
            onNew={handleNew}
            refreshKey={sessionsKey}
          />
        ) : (
          <>
            <MessageList
              messages={messages}
              streaming={streaming}
              toolEvents={toolEvents}
              busy={busy}
              onReload={activeId ? () => reloadMessages(activeId) : undefined}
            />
            {canRegenerate && (
              <div className="border-border flex justify-center border-t bg-background/60 px-3 py-1.5 backdrop-blur">
                <Button
                  variant="ghost"
                  size="sm"
                  className="text-muted-foreground hover:text-foreground gap-1.5 text-xs"
                  onClick={handleRegenerate}
                >
                  <RotateCcw className="size-3.5" />
                  {t('chat.regenerate')}
                </Button>
              </div>
            )}
            <Composer onSend={handleSend} onStop={handleStop} busy={busy} prefill={prefill} />
          </>
        )}
      </SheetContent>
    </Sheet>
  );
}
