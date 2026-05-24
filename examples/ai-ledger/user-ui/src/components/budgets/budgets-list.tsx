import { useTranslation } from 'react-i18next';
import type { BudgetStatus } from '@/lib/api';

function pct(used: string, limit: string): number {
  const u = Number(used);
  const l = Number(limit);
  if (!Number.isFinite(u) || !Number.isFinite(l) || l <= 0) return 0;
  return Math.max(0, Math.min(100, (u / l) * 100));
}

export function BudgetsList({ budgets }: { budgets: BudgetStatus[] }) {
  const { t } = useTranslation();

  if (budgets.length === 0) {
    return (
      <p className="text-muted-foreground py-6 text-center text-sm">{t('budgets.empty')}</p>
    );
  }

  return (
    <ul className="space-y-3">
      {budgets.map((b) => {
        const p = pct(b.used, b.limit);
        const bar = b.over_budget ? 'bg-rose-500' : 'bg-emerald-500';
        return (
          <li key={`${b.category}-${b.currency}`} className="space-y-1">
            <div className="flex items-center justify-between gap-2 text-sm">
              <span className="font-medium">{b.category}</span>
              <span className="text-muted-foreground tabular-nums text-xs">
                {b.used} / {b.limit} {b.currency}
              </span>
            </div>
            <div className="bg-muted h-2 w-full overflow-hidden rounded-full">
              <div
                className={`h-full ${bar} transition-all`}
                style={{ width: `${p}%` }}
                aria-valuenow={p}
                aria-valuemin={0}
                aria-valuemax={100}
                role="progressbar"
              />
            </div>
            {b.over_budget && (
              <div className="text-rose-600 dark:text-rose-400 text-xs">
                {t('budgets.over')}
              </div>
            )}
          </li>
        );
      })}
    </ul>
  );
}
