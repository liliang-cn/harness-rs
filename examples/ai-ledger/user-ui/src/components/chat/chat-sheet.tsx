import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { ArrowLeft, Plus, X } from 'lucide-react';
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
  const abortRef = useRef<AbortController | null>(null);
  const toolIdRef = useRef(0);

  // Load messages whenever a session is selected.
  useEffect(() => {
    if (!activeId) {
      setSession(null);
      setMessages([]);
      setStreaming(null);
      setToolEvents([]);
      return;
    }
    let cancelled = false;
    ledgerApi
      .getChatSession(activeId)
      .then((j) => {
        if (cancelled) return;
        setSession(j.session);
        setMessages(j.messages);
      })
      .catch((e) => {
        if (!cancelled) toast.error((e as Error).message);
      });
    return () => {
      cancelled = true;
    };
  }, [activeId]);

  // If the sheet closes mid-stream, abort the fetch so we don't keep the
  // generator running in the background.
  useEffect(() => {
    if (!open) {
      abortRef.current?.abort();
      abortRef.current = null;
    }
  }, [open]);

  const handleNew = useCallback(async () => {
    try {
      const j = await ledgerApi.createChatSession();
      setActiveId(j.session.id);
      setSessionsKey((k) => k + 1);
    } catch (e) {
      toast.error((e as Error).message);
    }
  }, []);

  const handleBack = useCallback(() => {
    abortRef.current?.abort();
    abortRef.current = null;
    setActiveId(null);
    setBusy(false);
    setSessionsKey((k) => k + 1);
  }, []);

  const handleStop = useCallback(() => {
    abortRef.current?.abort();
    abortRef.current = null;
  }, []);

  const handleSend = useCallback(
    async (text: string) => {
      if (!activeId || busy) return;
      const optimistic: ChatMessage = {
        id: `local-${Date.now()}`,
        session_id: activeId,
        role: 'user',
        text,
        created_at: new Date().toISOString(),
      };
      setMessages((cur) => [...cur, optimistic]);
      setStreaming('');
      setToolEvents([]);
      setBusy(true);
      const ctrl = new AbortController();
      abortRef.current = ctrl;
      let buf = '';
      let finalReply = '';
      let gotDone = false;

      await streamSession(
        activeId,
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
      );

      // Commit the assistant message. Use server-provided `reply` if non-empty
      // (it includes any post-processing); otherwise fall back to accumulated
      // deltas. ID prefix `stream-` keys this row distinctly until next refresh.
      const replyText = finalReply || buf;
      if (replyText) {
        const assistant: ChatMessage = {
          id: `stream-${Date.now()}`,
          session_id: activeId,
          role: 'asst',
          text: replyText,
          created_at: new Date().toISOString(),
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
    [activeId, busy, t],
  );

  const showSessions = !activeId;

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
            />
            <Composer onSend={handleSend} onStop={handleStop} busy={busy} />
          </>
        )}
      </SheetContent>
    </Sheet>
  );
}
