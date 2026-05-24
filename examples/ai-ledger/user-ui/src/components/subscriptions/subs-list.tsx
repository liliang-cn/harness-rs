import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import { Button } from '@/components/ui/button';
import type { Subscription } from '@/lib/api';

interface Props {
  subs: Subscription[];
  onCancel: (s: Subscription) => void;
  cancellingId?: string | null;
}

export function SubsList({ subs, onCancel, cancellingId }: Props) {
  const { t } = useTranslation();

  if (subs.length === 0) {
    return (
      <p className="text-muted-foreground py-6 text-center text-sm">{t('subs.empty')}</p>
    );
  }

  return (
    <ul className="divide-border divide-y">
      {subs.map((s) => (
        <li key={s.id} className="flex items-center justify-between gap-3 py-3 first:pt-0 last:pb-0">
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-2">
              <span className="truncate text-sm font-medium">{s.name}</span>
              <Badge variant="outline" className="text-xs">
                {t(`subs.freq.${s.frequency}`)}
              </Badge>
            </div>
            <div className="text-muted-foreground mt-0.5 text-xs">
              {t('subs.next', { date: s.next_charge_date })}
              {s.category ? ` · ${s.category}` : ''}
            </div>
          </div>
          <div className="flex shrink-0 items-center gap-2">
            <span className="text-sm tabular-nums">
              {s.amount} {s.currency}
            </span>
            <Button
              size="sm"
              variant="outline"
              onClick={() => onCancel(s)}
              disabled={cancellingId === s.id}
            >
              {cancellingId === s.id ? t('common.loading') : t('subs.cancel')}
            </Button>
          </div>
        </li>
      ))}
    </ul>
  );
}
