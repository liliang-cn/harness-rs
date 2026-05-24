import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { TrendingDown, TrendingUp, RotateCw } from 'lucide-react';
import { toast } from 'sonner';
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Skeleton } from '@/components/ui/skeleton';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import {
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from '@/components/ui/chart';
import { Area, AreaChart, XAxis, YAxis } from 'recharts';
import { ledgerApi, type NetWorthSnapshot, type Account } from '@/lib/api';

const TRACKED_CURRENCIES = [
  'USD', 'EUR', 'GBP', 'JPY', 'CNY', 'HKD', 'SGD', 'AUD', 'CAD', 'CHF', 'KRW',
];

const CURRENCY_SYMBOL: Record<string, string> = {
  USD: '$', EUR: '€', GBP: '£', JPY: '¥', CNY: '¥',
  HKD: 'HK$', SGD: 'S$', AUD: 'A$', CAD: 'C$', CHF: 'CHF ', KRW: '₩',
};

function formatMoney(amt: number, ccy: string): string {
  const noDecimals = ccy === 'JPY' || ccy === 'KRW';
  return (
    (CURRENCY_SYMBOL[ccy] ?? `${ccy} `) +
    amt.toLocaleString(undefined, {
      minimumFractionDigits: noDecimals ? 0 : 2,
      maximumFractionDigits: noDecimals ? 0 : 2,
    })
  );
}

const chartConfig = {
  net: {
    label: 'Net worth',
    color: 'var(--chart-1)',
  },
} satisfies ChartConfig;

export function Dashboard() {
  const { t, i18n } = useTranslation();
  const [snap, setSnap] = useState<NetWorthSnapshot | null>(null);
  const [series, setSeries] = useState<NetWorthSnapshot[]>([]);
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);

  async function loadAll() {
    setLoading(true);
    try {
      const [nw, s, a] = await Promise.all([
        ledgerApi.netWorth(),
        ledgerApi.netWorthSeries(),
        ledgerApi.accounts(),
      ]);
      setSnap(nw.snapshot);
      setSeries(s.series);
      setAccounts(a.accounts);
    } catch (e) {
      toast.error(`${t('common.error')}: ${(e as Error).message}`);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    loadAll();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  async function changeCurrency(ccy: string) {
    try {
      const r = await ledgerApi.setBaseCurrency(ccy);
      setSnap(r.snapshot);
      const s = await ledgerApi.netWorthSeries();
      setSeries(s.series);
    } catch (e) {
      toast.error(`${t('common.error')}: ${(e as Error).message}`);
    }
  }

  async function refreshNow() {
    setRefreshing(true);
    try {
      const r = await ledgerApi.netWorthRefresh();
      setSnap(r.snapshot);
      const s = await ledgerApi.netWorthSeries();
      setSeries(s.series);
    } catch (e) {
      toast.error(`${t('common.error')}: ${(e as Error).message}`);
    } finally {
      setRefreshing(false);
    }
  }

  if (loading || !snap) {
    return (
      <div className="space-y-6">
        <Skeleton className="h-44 w-full" />
        <Skeleton className="h-48 w-full" />
        <Skeleton className="h-72 w-full" />
      </div>
    );
  }

  const ccy = snap.base_currency;

  // 30d delta vs the earliest snapshot ≥ 30 days ago (or first row if shorter
  // history).
  let delta30Abs: number | null = null;
  let delta30Pct: number | null = null;
  if (series.length > 1) {
    const target = new Date(snap.snapshot_date);
    target.setDate(target.getDate() - 30);
    const targetISO = target.toISOString().slice(0, 10);
    const past = series.find((s) => s.snapshot_date >= targetISO) ?? series[0];
    if (past && past.net_amt !== 0) {
      delta30Abs = snap.net_amt - past.net_amt;
      delta30Pct = (delta30Abs / Math.abs(past.net_amt)) * 100;
    }
  }
  const up = (delta30Abs ?? 0) >= 0;

  const composition = [
    { label: t('dashboard.cash'), value: snap.cash_amt, color: 'bg-emerald-500' },
    { label: t('dashboard.investments'), value: snap.investments_amt, color: 'bg-sky-500' },
    { label: t('dashboard.debt'), value: -snap.debt_amt, color: 'bg-rose-500' },
  ];
  const totalAbs =
    Math.abs(snap.cash_amt) + Math.abs(snap.investments_amt) + Math.abs(snap.debt_amt);

  const chartData = series.map((s) => ({
    date: s.snapshot_date,
    net: Number(s.net_amt.toFixed(2)),
  }));

  return (
    <div className="space-y-6">
      {/* Net worth hero */}
      <Card>
        <CardHeader className="flex flex-row items-start justify-between space-y-0">
          <div>
            <CardDescription>{t('dashboard.title')}</CardDescription>
            <CardTitle className="mt-1 text-4xl font-bold tabular-nums">
              {formatMoney(snap.net_amt, ccy)}
            </CardTitle>
            <div className="mt-2 text-sm">
              {delta30Abs !== null && delta30Pct !== null ? (
                <span
                  className={cn_inline(
                    'inline-flex items-center gap-1',
                    up ? 'text-emerald-600 dark:text-emerald-400' : 'text-rose-600 dark:text-rose-400',
                  )}
                >
                  {up ? <TrendingUp className="size-4" /> : <TrendingDown className="size-4" />}
                  {t('dashboard.delta30', {
                    value: `${up ? '+' : ''}${formatMoney(delta30Abs, ccy)}`,
                    pct: `${up ? '+' : ''}${delta30Pct.toFixed(2)}%`,
                  })}
                </span>
              ) : (
                <span className="text-muted-foreground">{t('dashboard.noHistory')}</span>
              )}
            </div>
            <p className="text-muted-foreground mt-1 text-xs">
              {t('dashboard.asOf', {
                date: new Date(snap.snapshot_date).toLocaleDateString(i18n.language),
              })}
            </p>
          </div>
          <div className="flex items-center gap-2">
            <Select value={ccy} onValueChange={changeCurrency}>
              <SelectTrigger size="sm" className="w-28">
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
            <Button
              variant="outline"
              size="icon"
              onClick={refreshNow}
              disabled={refreshing}
              title={t('dashboard.refreshNow')}
            >
              <RotateCw className={refreshing ? 'animate-spin' : ''} />
            </Button>
          </div>
        </CardHeader>
      </Card>

      {/* Composition */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('dashboard.composition')}</CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          {composition.map((row) => {
            const pct = totalAbs > 0 ? (Math.abs(row.value) / totalAbs) * 100 : 0;
            return (
              <div key={row.label}>
                <div className="mb-1 flex items-center justify-between text-sm">
                  <span className="inline-flex items-center gap-2">
                    <span className={`size-2 rounded-full ${row.color}`} />
                    {row.label}
                  </span>
                  <span className="tabular-nums">
                    {formatMoney(row.value, ccy)}{' '}
                    <span className="text-muted-foreground text-xs">
                      ({pct.toFixed(1)}%)
                    </span>
                  </span>
                </div>
                <div className="bg-muted h-1.5 w-full overflow-hidden rounded-full">
                  <div
                    className={`h-full ${row.color} transition-all`}
                    style={{ width: `${pct}%` }}
                  />
                </div>
              </div>
            );
          })}
        </CardContent>
      </Card>

      {/* Trend chart */}
      {chartData.length > 1 && (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t('dashboard.trend12mo')}</CardTitle>
          </CardHeader>
          <CardContent>
            <ChartContainer config={chartConfig} className="h-56 w-full">
              <AreaChart data={chartData}>
                <defs>
                  <linearGradient id="netGradient" x1="0" y1="0" x2="0" y2="1">
                    <stop offset="0%" stopColor="var(--chart-1)" stopOpacity={0.4} />
                    <stop offset="95%" stopColor="var(--chart-1)" stopOpacity={0.05} />
                  </linearGradient>
                </defs>
                <XAxis
                  dataKey="date"
                  tickLine={false}
                  axisLine={false}
                  tickMargin={8}
                  tickFormatter={(d) =>
                    new Date(d).toLocaleDateString(i18n.language, {
                      month: 'short',
                      day: 'numeric',
                    })
                  }
                  style={{ fontSize: 11 }}
                />
                <YAxis
                  width={48}
                  tickLine={false}
                  axisLine={false}
                  tickFormatter={(v) => {
                    if (v >= 1e6) return `${(v / 1e6).toFixed(1)}M`;
                    if (v >= 1e3) return `${(v / 1e3).toFixed(0)}k`;
                    return String(v);
                  }}
                  style={{ fontSize: 11 }}
                />
                <ChartTooltip
                  content={
                    <ChartTooltipContent
                      formatter={(v) =>
                        typeof v === 'number' ? formatMoney(v, ccy) : String(v)
                      }
                    />
                  }
                />
                <Area
                  type="monotone"
                  dataKey="net"
                  stroke="var(--chart-1)"
                  strokeWidth={2}
                  fill="url(#netGradient)"
                />
              </AreaChart>
            </ChartContainer>
          </CardContent>
        </Card>
      )}

      {/* Accounts */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">
            {t('dashboard.accounts')} ({accounts.length})
          </CardTitle>
        </CardHeader>
        <CardContent>
          {accounts.length === 0 ? (
            <p className="text-muted-foreground text-sm">{t('dashboard.noAccounts')}</p>
          ) : (
            <ul className="divide-border divide-y">
              {accounts.map((a) => (
                <li
                  key={a.id}
                  className="flex items-center justify-between py-2.5 first:pt-0 last:pb-0"
                >
                  <div className="flex items-center gap-2">
                    <span className="font-medium">{a.name}</span>
                    <Badge variant="secondary">{a.kind}</Badge>
                    <span className="text-muted-foreground text-xs">{a.currency}</span>
                  </div>
                  <span className="tabular-nums">
                    {Number(a.opening_balance).toLocaleString()}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </CardContent>
      </Card>
    </div>
  );
}

// Tiny inline cn wrapper — Dashboard uses this in two spots and importing
// the shared one is overkill here.
function cn_inline(...xs: (string | undefined | false)[]) {
  return xs.filter(Boolean).join(' ');
}
