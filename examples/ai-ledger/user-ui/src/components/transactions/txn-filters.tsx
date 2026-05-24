import { useTranslation } from 'react-i18next';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';

export type Period = 'thisMonth' | 'last30' | 'thisYear' | 'allTime';

const PERIODS: Period[] = ['thisMonth', 'last30', 'thisYear', 'allTime'];

export function TxnFilters({
  period,
  onPeriodChange,
}: {
  period: Period;
  onPeriodChange: (p: Period) => void;
}) {
  const { t } = useTranslation();
  return (
    <Select value={period} onValueChange={(v) => onPeriodChange(v as Period)}>
      <SelectTrigger size="sm" className="w-32 sm:w-36">
        <SelectValue />
      </SelectTrigger>
      <SelectContent align="end">
        {PERIODS.map((p) => (
          <SelectItem key={p} value={p}>
            {t(`ledger.${p}`)}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

/** Returns inclusive `from`/exclusive `to` as YYYY-MM-DD for the period. */
export function periodToRange(p: Period): { from?: string; to?: string } {
  const now = new Date();
  const iso = (d: Date) => d.toISOString().slice(0, 10);
  switch (p) {
    case 'thisMonth':
      return { from: iso(new Date(now.getFullYear(), now.getMonth(), 1)) };
    case 'last30':
      return { from: iso(new Date(now.getTime() - 30 * 86_400_000)) };
    case 'thisYear':
      return { from: iso(new Date(now.getFullYear(), 0, 1)) };
    case 'allTime':
      return {};
  }
}
