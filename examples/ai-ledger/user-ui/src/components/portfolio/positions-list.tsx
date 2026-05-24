import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import type { Position } from '@/lib/api';

function fmtNum(s: string | null | undefined, maxFrac = 4): string {
  if (s === null || s === undefined || s === '') return '—';
  const n = Number(s);
  if (!Number.isFinite(n)) return s;
  return n.toLocaleString(undefined, {
    minimumFractionDigits: 0,
    maximumFractionDigits: maxFrac,
  });
}

function fmtMoney(s: string | null | undefined): string {
  if (s === null || s === undefined || s === '') return '—';
  const n = Number(s);
  if (!Number.isFinite(n)) return s;
  return n.toLocaleString(undefined, {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  });
}

export function PositionsList({ positions }: { positions: Position[] }) {
  const { t } = useTranslation();

  // Filter out closed (qty <= 0) — they pollute the main list.
  const open = positions.filter((p) => Number(p.qty) > 0);

  if (open.length === 0) {
    return (
      <p className="text-muted-foreground py-8 text-center text-sm">
        {t('portfolio.empty')}
      </p>
    );
  }

  return (
    <ul className="divide-border divide-y">
      {open.map((p) => {
        const upl = p.unrealized_pl;
        const uplNum = upl !== null ? Number(upl) : null;
        const uplClass =
          uplNum === null
            ? 'text-muted-foreground'
            : uplNum >= 0
              ? 'text-emerald-600 dark:text-emerald-400'
              : 'text-rose-600 dark:text-rose-400';
        const mv = p.market_value;
        return (
          <li key={p.asset_id} className="py-3 first:pt-0 last:pb-0">
            <div className="flex items-start justify-between gap-2">
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-medium">{p.symbol}</span>
                  <Badge variant="secondary" className="text-[10px] uppercase">
                    {p.asset_class}
                  </Badge>
                  <span className="text-muted-foreground text-xs">
                    {p.currency}
                  </span>
                </div>
                <div className="text-muted-foreground mt-0.5 truncate text-xs">
                  {p.name}
                </div>
                <div className="text-muted-foreground mt-1 flex flex-wrap gap-x-3 gap-y-0.5 text-xs">
                  <span>
                    {t('portfolio.qty')}:{' '}
                    <span className="text-foreground tabular-nums">
                      {fmtNum(p.qty)}
                    </span>
                  </span>
                  <span>
                    {t('portfolio.avg')}:{' '}
                    <span className="text-foreground tabular-nums">
                      {fmtMoney(p.avg_cost)}
                    </span>
                  </span>
                </div>
              </div>
              <div className="shrink-0 text-right">
                <div className="text-sm font-medium tabular-nums">
                  {mv === null ? (
                    <span className="text-muted-foreground text-xs italic">
                      {t('portfolio.noPrice')}
                    </span>
                  ) : (
                    <>
                      {fmtMoney(mv)}{' '}
                      <span className="text-muted-foreground text-xs">
                        {p.currency}
                      </span>
                    </>
                  )}
                </div>
                {upl !== null && (
                  <div className={`mt-0.5 text-xs tabular-nums ${uplClass}`}>
                    {uplNum !== null && uplNum >= 0 ? '+' : ''}
                    {fmtMoney(upl)}
                  </div>
                )}
              </div>
            </div>
          </li>
        );
      })}
    </ul>
  );
}
