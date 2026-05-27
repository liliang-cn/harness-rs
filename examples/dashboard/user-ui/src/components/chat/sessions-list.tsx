import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { format, parseISO } from 'date-fns';
import { Plus, Trash2 } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import { ledgerApi, type ChatSession } from '@/lib/api';

interface SessionsListProps {
  onSelect: (id: string) => void;
  onNew: () => void;
  /** bumped by ChatSheet whenever a session is created/deleted so the list refetches */
  refreshKey: number;
}

export function SessionsList({ onSelect, onNew, refreshKey }: SessionsListProps) {
  const { t } = useTranslation();
  const [sessions, setSessions] = useState<ChatSession[] | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);

  // Per-session "last seen message_count" — used to compute the +N unread
  // badge for the case where the user closed mid-stream and the agent
  // finished in the background, bumping message_count past what the
  // client last saw.
  const [seenCount, setSeenCount] = useState<Record<string, number>>(() => {
    try {
      return JSON.parse(localStorage.getItem('chat-seen-count') ?? '{}');
    } catch {
      return {};
    }
  });

  useEffect(() => {
    let cancelled = false;
    setSessions(null);
    ledgerApi
      .chatSessions()
      .then((j) => {
        if (cancelled) return;
        // Empty sessions are noise in the list — every "+ new chat"
        // historically created a row before the user typed anything.
        // We now defer creation in ChatSheet, but legacy rows + any race
        // (sheet closed mid-create) leave stale 0-message entries in
        // the DB. Hide them at render time.
        setSessions(j.sessions.filter((s) => s.message_count > 0));
        // Re-read seen counts on every refresh — ChatSheet writes the
        // map after each successful load so an updated count gets
        // reflected here next time the picker shows.
        try {
          setSeenCount(JSON.parse(localStorage.getItem('chat-seen-count') ?? '{}'));
        } catch {
          /* ignore */
        }
      })
      .catch(() => {
        if (!cancelled) setSessions([]);
      });
    return () => {
      cancelled = true;
    };
  }, [refreshKey]);

  async function handleDelete(e: React.MouseEvent, id: string) {
    e.stopPropagation();
    if (!confirm(t('chat.deleteConfirm'))) return;
    setBusyId(id);
    try {
      await ledgerApi.deleteChatSession(id);
      setSessions((cur) => cur?.filter((s) => s.id !== id) ?? null);
      toast.success(t('chat.deleted'));
    } catch (err) {
      toast.error((err as Error).message);
    } finally {
      setBusyId(null);
    }
  }

  return (
    <div className="flex h-full flex-col">
      <div className="border-border flex items-center justify-between border-b px-4 py-3">
        <span className="text-muted-foreground text-xs">
          {sessions ? sessions.length : ''}
        </span>
        <Button size="sm" onClick={onNew}>
          <Plus className="size-4" /> {t('chat.newChat')}
        </Button>
      </div>
      <div className="flex-1 overflow-y-auto">
        {sessions === null ? (
          <div className="space-y-2 p-4">
            <Skeleton className="h-12 w-full" />
            <Skeleton className="h-12 w-full" />
            <Skeleton className="h-12 w-full" />
          </div>
        ) : sessions.length === 0 ? (
          <div className="text-muted-foreground p-6 text-center text-sm">
            {t('chat.sessionsEmpty')}
          </div>
        ) : (
          <ul className="divide-border divide-y">
            {sessions.map((s) => {
              const seen = seenCount[s.id] ?? 0;
              const unread = Math.max(0, s.message_count - seen);
              return (
              <li key={s.id}>
                <button
                  type="button"
                  onClick={() => onSelect(s.id)}
                  className="hover:bg-accent flex w-full items-center gap-2 px-4 py-3 text-left"
                >
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2">
                      <span className="truncate text-sm font-medium">
                        {s.title?.trim() || t('chat.untitled')}
                      </span>
                      {unread > 0 && (
                        <span
                          aria-label={t('chat.unread', { defaultValue: 'unread' })}
                          className="bg-primary text-primary-foreground inline-flex h-5 min-w-5 items-center justify-center rounded-full px-1.5 text-[10px] font-medium"
                        >
                          {unread > 9 ? '9+' : `+${unread}`}
                        </span>
                      )}
                    </div>
                    <div className="text-muted-foreground mt-0.5 flex gap-2 text-xs">
                      <span>
                        {t('chat.msgCount', { count: s.message_count })}
                      </span>
                      <span>·</span>
                      <span>{format(parseISO(s.updated_at), 'yyyy-MM-dd HH:mm')}</span>
                    </div>
                  </div>
                  <Button
                    variant="ghost"
                    size="icon-sm"
                    aria-label={t('chat.delete')}
                    disabled={busyId === s.id}
                    onClick={(e) => handleDelete(e, s.id)}
                    asChild={false}
                  >
                    <Trash2 className="size-4" />
                  </Button>
                </button>
              </li>
              );
            })}
          </ul>
        )}
      </div>
    </div>
  );
}
