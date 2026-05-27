import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import type { Trade, TradeKind } from '@/lib/api';

const KIND_TONE: Record<TradeKind, string> = {
  buy: 'text-emerald-600 dark:text-emerald-400 border-emerald-200 dark:border-emerald-900',
  opening:
    'text-emerald-600 dark:text-emerald-400 border-emerald-200 dark:border-emerald-900',
  sell: 'text-rose-600 dark:text-rose-400 border-rose-200 dark:border-rose-900',
};

function fmtNum(s: string, maxFrac = 4): string {
  const n = Number(s);
  if (!Number.isFinite(n)) return s;
  return n.toLocaleString(undefined, {
    minimumFractionDigits: 0,
    maximumFractionDigits: maxFrac,
  });
}

function fmtMoney(s: string): string {
  const n = Number(s);
  if (!Number.isFinite(n)) return s;
  return n.toLocaleString(undefined, {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  });
}

export function TradesList({
  trades,
  symbolByAssetId,
}: {
  trades: Trade[];
  /** asset_id → ticker symbol for display. Falls back to the raw asset_id. */
  symbolByAssetId: Map<string, string>;
}) {
  const { t, i18n } = useTranslation();

  if (trades.length === 0) {
    return (
      <p className="text-muted-foreground py-8 text-center text-sm">
        {t('portfolio.empty')}
      </p>
    );
  }

  // Group by YYYY-MM-DD descending.
  const groups = new Map<string, Trade[]>();
  for (const tr of trades) {
    const k = tr.occurred_at.slice(0, 10);
    const g = groups.get(k) ?? [];
    g.push(tr);
    groups.set(k, g);
  }
  const sortedKeys = [...groups.keys()].sort().reverse();

  return (
    <div className="divide-border divide-y">
      {sortedKeys.map((dateKey) => (
        <div key={dateKey} className="py-3 first:pt-0 last:pb-0">
          <div className="text-muted-foreground mb-1 text-xs">
            {new Date(dateKey).toLocaleDateString(i18n.language, {
              year: 'numeric',
              month: 'short',
              day: 'numeric',
              weekday: 'short',
            })}
          </div>
          <ul className="space-y-1">
            {groups.get(dateKey)!.map((tr) => {
              const sym = symbolByAssetId.get(tr.asset_id) ?? tr.asset_id;
              return (
                <li
                  key={tr.id}
                  className="hover:bg-muted/40 flex items-center justify-between gap-2 rounded-md px-2 py-2"
                >
                  <div className="min-w-0 flex-1">
                    <div className="flex flex-wrap items-center gap-2">
                      <Badge variant="outline" className={KIND_TONE[tr.kind]}>
                        {t(`portfolio.${tr.kind}`)}
                      </Badge>
                      <span className="font-medium">{sym}</span>
                    </div>
                    {tr.note && (
                      <div className="text-muted-foreground mt-0.5 truncate text-xs">
                        {tr.note}
                      </div>
                    )}
                  </div>
                  <div className="shrink-0 text-right text-sm tabular-nums">
                    {fmtNum(tr.qty)}{' '}
                    <span className="text-muted-foreground text-xs">@</span>{' '}
                    {fmtMoney(tr.price_per_unit)}{' '}
                    <span className="text-muted-foreground text-xs">
                      {tr.currency}
                    </span>
                  </div>
                </li>
              );
            })}
          </ul>
        </div>
      ))}
    </div>
  );
}
