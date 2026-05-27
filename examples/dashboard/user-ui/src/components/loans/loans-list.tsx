import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import { Skeleton } from '@/components/ui/skeleton';
import { cn } from '@/lib/utils';
import type { Loan } from '@/lib/api';

interface Props {
  loans: Loan[] | null;
}

function kindLabelKey(kind: Loan['kind']): string {
  switch (kind) {
    case 'mortgage':
      return 'loans.kindMortgage';
    case 'receivable':
      return 'loans.kindReceivable';
    default:
      return 'loans.kindLoan';
  }
}

// "0.045" → "4.50"
function formatApr(raw: string): string {
  const n = Number(raw);
  if (!Number.isFinite(n)) return raw;
  return (n * 100).toFixed(2);
}

function clampPct(p: number): number {
  if (!Number.isFinite(p)) return 0;
  if (p < 0) return 0;
  if (p > 100) return 100;
  return p;
}

export function LoansList({ loans }: Props) {
  const { t } = useTranslation();

  // Loading
  if (loans === null) {
    return (
      <div className="space-y-2">
        <Skeleton className="h-16 w-full" />
        <Skeleton className="h-16 w-full" />
        <Skeleton className="h-16 w-full" />
      </div>
    );
  }

  // Filter to active only (paid-off ones are hidden in v1)
  const active = loans.filter((l) => l.status === 'active');

  if (active.length === 0) {
    return (
      <p className="text-muted-foreground py-6 text-center text-sm">
        {t('loans.empty')}
      </p>
    );
  }

  return (
    <ul className="divide-border divide-y">
      {active.map((l) => {
        const isReceivable = l.kind === 'receivable';
        const barColor = isReceivable ? 'bg-emerald-500' : 'bg-rose-500';
        const remainingColor = isReceivable
          ? 'text-emerald-600 dark:text-emerald-400'
          : '';
        const pct = clampPct(l.progress_pct);
        const aprDisplay = formatApr(l.apr);

        return (
          <li
            key={l.account_id}
            className="space-y-2 py-3 first:pt-0 last:pb-0"
          >
            {/* Row 1: kind + name + counterparty + remaining */}
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2">
                  <Badge variant="outline" className="text-xs">
                    {t(kindLabelKey(l.kind))}
                  </Badge>
                  <span className="truncate text-sm font-medium">{l.name}</span>
                </div>
                {l.counterparty ? (
                  <div className="text-muted-foreground mt-0.5 truncate text-xs">
                    {isReceivable ? t('loans.owedToYou') : l.counterparty}
                    {isReceivable ? ` · ${l.counterparty}` : ''}
                  </div>
                ) : null}
              </div>
              <div className="shrink-0 text-right">
                <div className={cn('text-sm tabular-nums font-medium', remainingColor)}>
                  {l.remaining} {l.currency}
                </div>
                <div className="text-muted-foreground text-xs">
                  {t('loans.remaining')}
                </div>
              </div>
            </div>

            {/* Row 2: progress bar */}
            <div
              className="bg-muted h-1.5 w-full overflow-hidden rounded-full"
              role="progressbar"
              aria-valuenow={pct}
              aria-valuemin={0}
              aria-valuemax={100}
            >
              <div
                className={cn('h-full transition-all', barColor)}
                style={{ width: `${pct}%` }}
              />
            </div>

            {/* Row 3: APR / monthly / next-due */}
            <div className="text-muted-foreground flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
              <span>{t('loans.apr', { rate: aprDisplay })}</span>
              {l.monthly_payment ? (
                <span>
                  {t('loans.monthlyPayment')}: {l.monthly_payment} {l.currency}
                </span>
              ) : null}
              {l.next_due_date ? (
                <span>{t('loans.nextDue', { date: l.next_due_date })}</span>
              ) : null}
            </div>
          </li>
        );
      })}
    </ul>
  );
}
