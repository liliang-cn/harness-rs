import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';

export const TRACKED_CURRENCIES = [
  'USD', 'EUR', 'GBP', 'JPY', 'CNY', 'HKD', 'SGD', 'AUD', 'CAD', 'CHF', 'KRW',
];

export const CURRENCY_SYMBOL: Record<string, string> = {
  USD: '$', EUR: '€', GBP: '£', JPY: '¥', CNY: '¥',
  HKD: 'HK$', SGD: 'S$', AUD: 'A$', CAD: 'C$', CHF: 'CHF ', KRW: '₩',
};

export function CurrencyPicker({
  value,
  onChange,
  size = 'sm',
}: {
  value: string;
  onChange: (v: string) => void;
  size?: 'sm' | 'default';
}) {
  return (
    <Select value={value} onValueChange={onChange}>
      <SelectTrigger size={size} className="w-28">
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {TRACKED_CURRENCIES.map((c) => (
          <SelectItem key={c} value={c}>
            {c} {CURRENCY_SYMBOL[c] ?? ''}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
