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
  noteApi,
  type ChatMessage,
  type ChatSession,
} from '@/lib/api';
import { useSpace } from '@/components/space-context';
import { SessionsList } from './sessions-list';
import { MessageList, type ToolEvent } from './message-list';
import { Composer } from './composer';
import { streamSession } from './stream';

interface ChatSheetProps {
  open: boolean;
  onOpenChange: (v: boolean) => void;
}

export function ChatSheet({ open, onOpenChange }: ChatSheetProps) {
  const { t, i18n } = useTranslation();
  const { space } = useSpace();
  const [activeId, setActiveId] = useState<string | null>(null);
  const [session, setSession] = useState<ChatSession | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [streaming, setStreaming] = useState<string | null>(null);
  const [toolEvents, setToolEvents] = useState<ToolEvent[]>([]);
  const [busy, setBusy] = useState(false);
  const [sessionsKey, setSessionsKey] = useState(0);
  const [drafting, setDrafting] = useState(false);
  const abortRef = useRef<AbortController | null>(null);
  const toolIdRef = useRef(0);
  const skipNextLoad = useRef(false);

  const reloadMessages = useCallback(async (id: string) => {
    try {
      const j = await noteApi.getChatSession(id);
      setSession(j.session);
      setMessages(j.messages);
      try {
        const raw = localStorage.getItem('chat-seen-count') ?? '{}';
        const seen = JSON.parse(raw) as Record<string, number>;
        seen[id] = j.session.message_count;
        localStorage.setItem('chat-seen-count', JSON.stringify(seen));
      } catch {
        /* swallow */
      }
    } catch (e) {
      toast.error((e as Error).message);
    }
  }, []);

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
    noteApi
      .getChatSession(activeId)
      .then((j: { session: ChatSession; messages: ChatMessage[] }) => {
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
      .catch((e: Error) => {
        if (!cancelled) toast.error(e.message);
      });
    return () => {
      cancelled = true;
    };
  }, [activeId]);

  useEffect(() => {
    if (!open) {
      abortRef.current?.abort();
      abortRef.current = null;
      setDrafting(false);
    }
  }, [open]);

  // When the user switches space, reset to the sessions list for the new space.
  const prevSpaceRef = useRef(space);
  useEffect(() => {
    if (prevSpaceRef.current !== space) {
      prevSpaceRef.current = space;
      abortRef.current?.abort();
      abortRef.current = null;
      setActiveId(null);
      setSession(null);
      setMessages([]);
      setStreaming(null);
      setToolEvents([]);
      setDrafting(false);
      setBusy(false);
      setSessionsKey((k) => k + 1);
    }
  }, [space]);

  const wasOpenRef = useRef(open);
  useEffect(() => {
    if (open && !wasOpenRef.current && activeId && !busy) {
      reloadMessages(activeId);
    }
    wasOpenRef.current = open;
  }, [open, activeId, busy, reloadMessages]);

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
      opts: { regenerate?: boolean } = {},
    ) => {
      if (busy) return;
      let sessionId = activeId;
      if (!sessionId) {
        try {
          const j = await noteApi.createChatSession(space);
          sessionId = j.session.id;
          skipNextLoad.current = true;
          setActiveId(sessionId);
          setDrafting(false);
        } catch (e) {
          toast.error((e as Error).message);
          return;
        }
      }
      if (!opts.regenerate) {
        const optimistic: ChatMessage = {
          id: `local-${Date.now()}`,
          session_id: sessionId,
          role: 'user',
          text,
          created_at: new Date().toISOString(),
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
      );

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

      setSessionsKey((k) => k + 1);
    },
    [activeId, busy, space, t, i18n.language],
  );

  const handleRegenerate = useCallback(() => {
    if (busy) return;
    let lastUserIdx = -1;
    for (let i = messages.length - 1; i >= 0; i--) {
      if (messages[i].role === 'user') {
        lastUserIdx = i;
        break;
      }
    }
    if (lastUserIdx < 0) return;
    const lastUserText = messages[lastUserIdx].text;
    setMessages((cur) => cur.slice(0, lastUserIdx + 1));
    handleSend(lastUserText, { regenerate: true });
  }, [busy, messages, handleSend]);

  const canRegenerate =
    !!activeId &&
    !busy &&
    messages.some((m) => m.role === 'user');

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
            space={space}
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
            <Composer onSend={handleSend} onStop={handleStop} busy={busy} />
          </>
        )}
      </SheetContent>
    </Sheet>
  );
}
