import { useTranslation } from 'react-i18next';
import { ArrowLeftRight } from 'lucide-react';
import type { Transaction } from '@/lib/api';
import { cn } from '@/lib/utils';

export function TxnList({ txns }: { txns: Transaction[] }) {
  const { t, i18n } = useTranslation();

  if (txns.length === 0) {
    return (
      <p className="text-muted-foreground py-8 text-center text-sm">{t('ledger.empty')}</p>
    );
  }

  // Group by YYYY-MM-DD descending.
  const groups = new Map<string, Transaction[]>();
  for (const tx of txns) {
    const k = tx.occurred_at.slice(0, 10);
    const g = groups.get(k) ?? [];
    g.push(tx);
    groups.set(k, g);
  }
  const sortedKeys = [...groups.keys()].sort().reverse();

  return (
    <div className="space-y-5">
      {sortedKeys.map((dateKey) => (
        <section key={dateKey}>
          <header className="text-muted-foreground mb-2 text-xs font-medium tracking-wide uppercase">
            {new Date(dateKey).toLocaleDateString(i18n.language, {
              year: 'numeric',
              month: 'short',
              day: 'numeric',
              weekday: 'short',
            })}
          </header>
          <ul className="divide-border divide-y rounded-lg border">
            {groups.get(dateKey)!.map((tx) => (
              <Row key={tx.id} tx={tx} />
            ))}
          </ul>
        </section>
      ))}
    </div>
  );
}

function Row({ tx }: { tx: Transaction }) {
  // Primary text: the user's own description if they wrote one ("买域名",
  // "冰粉"), otherwise fall back to the category. That keeps the row
  // anchored to the specific thing rather than the bucket.
  const primary = tx.note?.trim() || tx.category?.trim() || '—';
  const secondary = tx.note?.trim() ? tx.category?.trim() : null;

  const isExpense = tx.kind === 'expense';
  const isIncome = tx.kind === 'income';
  const sign = isExpense ? '-' : isIncome ? '+' : '';
  const amountTone = isExpense
    ? 'text-rose-600 dark:text-rose-400'
    : isIncome
      ? 'text-emerald-600 dark:text-emerald-400'
      : 'text-foreground';

  return (
    <li className="hover:bg-muted/40 flex items-center justify-between gap-3 px-3 py-2.5 transition-colors first:rounded-t-lg last:rounded-b-lg">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-1.5 text-sm">
          {tx.kind === 'transfer' && (
            <ArrowLeftRight className="text-muted-foreground size-3.5 shrink-0" />
          )}
          <span className="truncate font-medium">{primary}</span>
        </div>
        {secondary && (
          <div className="text-muted-foreground mt-0.5 truncate text-xs">{secondary}</div>
        )}
      </div>
      <span className={cn('text-sm tabular-nums whitespace-nowrap', amountTone)}>
        {sign}
        {tx.amount} <span className="text-muted-foreground text-xs">{tx.currency}</span>
      </span>
    </li>
  );
}
