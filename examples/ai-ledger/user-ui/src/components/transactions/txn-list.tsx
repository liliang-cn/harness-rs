import { useTranslation } from 'react-i18next';
import { Badge } from '@/components/ui/badge';
import type { Transaction } from '@/lib/api';

const KIND_TONE: Record<Transaction['kind'], string> = {
  income: 'text-emerald-600 dark:text-emerald-400 border-emerald-200 dark:border-emerald-900',
  expense: 'text-rose-600 dark:text-rose-400 border-rose-200 dark:border-rose-900',
  transfer: 'text-muted-foreground border-border',
};

export function TxnList({ txns }: { txns: Transaction[] }) {
  const { t, i18n } = useTranslation();

  if (txns.length === 0) {
    return (
      <p className="text-muted-foreground py-8 text-center text-sm">{t('ledger.empty')}</p>
    );
  }

  // Group by YYYY-MM-DD
  const groups = new Map<string, Transaction[]>();
  for (const tx of txns) {
    const k = tx.occurred_at.slice(0, 10);
    const g = groups.get(k) ?? [];
    g.push(tx);
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
            {groups.get(dateKey)!.map((tx) => (
              <li
                key={tx.id}
                className="hover:bg-muted/40 flex items-center justify-between gap-2 rounded-md px-2 py-2"
              >
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <Badge variant="outline" className={KIND_TONE[tx.kind]}>
                      {t(`ledger.${tx.kind}`)}
                    </Badge>
                    <span className="text-muted-foreground truncate text-xs">
                      {tx.category ?? '—'}
                    </span>
                  </div>
                  {tx.note && (
                    <div className="text-muted-foreground mt-0.5 truncate text-xs">
                      {tx.note}
                    </div>
                  )}
                </div>
                <div className="flex shrink-0 items-center gap-1">
                  <span
                    className={`text-sm tabular-nums ${
                      tx.kind === 'expense'
                        ? 'text-rose-600 dark:text-rose-400'
                        : tx.kind === 'income'
                          ? 'text-emerald-600 dark:text-emerald-400'
                          : ''
                    }`}
                  >
                    {tx.kind === 'expense' ? '-' : tx.kind === 'income' ? '+' : ''}
                    {tx.amount} {tx.currency}
                  </span>
                </div>
              </li>
            ))}
          </ul>
        </div>
      ))}
    </div>
  );
}
