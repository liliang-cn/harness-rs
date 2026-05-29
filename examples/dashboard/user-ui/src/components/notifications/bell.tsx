import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Bell } from 'lucide-react';
import {
  Popover, PopoverContent, PopoverTrigger,
} from '@/components/ui/popover';
import { ledgerApi, type NotificationItem } from '@/lib/api';

function DigestBody({ body }: { body: any }) {
  if (!body || typeof body !== 'object') return null;
  const s = body.spending;
  const w = body.wealth;
  const m = body.market;
  return (
    <div className="space-y-1 text-xs text-muted-foreground">
      {s && <div>昨日支出 {Number(s.total).toFixed(2)} {s.currency}</div>}
      {w && <div>净值 {Number(w.net_worth).toFixed(2)} {w.currency}（{Number(w.net_delta) >= 0 ? '+' : ''}{Number(w.net_delta).toFixed(2)}）</div>}
      {m?.summary && <div>{m.summary}</div>}
    </div>
  );
}

export function NotificationBell() {
  const { t } = useTranslation();
  const [items, setItems] = useState<NotificationItem[]>([]);
  const [unread, setUnread] = useState(0);
  const [open, setOpen] = useState(false);

  const load = useCallback(() => {
    ledgerApi.notifications(false).then((r) => {
      setItems(r.notifications);
      setUnread(r.unread);
    }).catch(() => {});
  }, []);

  useEffect(() => {
    load();
    const id = setInterval(load, 5 * 60 * 1000);
    const onFocus = () => load();
    window.addEventListener('focus', onFocus);
    return () => { clearInterval(id); window.removeEventListener('focus', onFocus); };
  }, [load]);

  async function onOpenChange(next: boolean) {
    setOpen(next);
    if (next && unread > 0) {
      await ledgerApi.markNotificationsRead().catch(() => {});
      setUnread(0);
    }
  }

  return (
    <Popover open={open} onOpenChange={onOpenChange}>
      <PopoverTrigger className="relative inline-flex h-9 w-9 items-center justify-center rounded-md hover:bg-accent" aria-label={t('notifications.title')}>
        <Bell className="size-4" />
        {unread > 0 && (
          <span className="absolute -right-0.5 -top-0.5 flex h-4 min-w-4 items-center justify-center rounded-full bg-red-500 px-1 text-[10px] font-medium text-white">
            {unread > 9 ? '9+' : unread}
          </span>
        )}
      </PopoverTrigger>
      <PopoverContent align="end" className="w-80 p-0">
        <div className="border-b px-3 py-2 text-sm font-medium">{t('notifications.title')}</div>
        <div className="max-h-96 overflow-y-auto">
          {items.length === 0 ? (
            <div className="px-3 py-6 text-center text-sm text-muted-foreground">{t('notifications.empty')}</div>
          ) : (
            items.map((n) => (
              <div key={n.id} className="border-b px-3 py-2 last:border-b-0">
                <div className="text-sm font-medium">{n.title}{n.body?.date ? ` · ${n.body.date}` : ''}</div>
                <DigestBody body={n.body} />
              </div>
            ))
          )}
        </div>
      </PopoverContent>
    </Popover>
  );
}
