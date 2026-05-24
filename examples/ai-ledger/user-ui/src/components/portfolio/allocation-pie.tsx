import { useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Cell, Pie, PieChart } from 'recharts';
import {
  ChartContainer,
  ChartLegend,
  ChartLegendContent,
  ChartTooltip,
  ChartTooltipContent,
  type ChartConfig,
} from '@/components/ui/chart';
import { Skeleton } from '@/components/ui/skeleton';
import { ledgerApi, type AssetClass } from '@/lib/api';

const CLASS_ORDER: AssetClass[] = [
  'stock',
  'etf',
  'crypto',
  'commodity',
  'other',
];

// chart-1 … chart-5 are tuned in index.css for both themes.
const CLASS_COLOR: Record<AssetClass, string> = {
  stock: 'var(--chart-1)',
  etf: 'var(--chart-2)',
  crypto: 'var(--chart-3)',
  commodity: 'var(--chart-4)',
  other: 'var(--chart-5)',
};

interface AllocationRow {
  class: string;
  value: number;
  pct: number;
}

// Component fetches its own data because the allocation values must be
// FX-converted to the user's base_currency — that conversion lives on the
// server (where the fx_rates cache is). Computing it client-side from the
// raw `positions` payload would over-weight high-nominal-number
// currencies (CNY 343k > USD 18k even though they're worth the same).
export function AllocationPie() {
  const { t } = useTranslation();
  const [data, setData] = useState<AllocationRow[] | null>(null);
  const [base, setBase] = useState<string>('USD');
  const [missing, setMissing] = useState<string[]>([]);

  useEffect(() => {
    let cancelled = false;
    ledgerApi
      .allocation()
      .then((r) => {
        if (cancelled) return;
        setData(r.by_class);
        setBase(r.base_currency);
        setMissing(r.missing_rate_for);
      })
      .catch(() => {
        if (!cancelled) setData([]);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const config = useMemo<ChartConfig>(() => {
    const cfg: ChartConfig = {};
    for (const c of CLASS_ORDER) {
      cfg[c] = { label: t(`portfolio.class.${c}`), color: CLASS_COLOR[c] };
    }
    return cfg;
  }, [t]);

  if (data === null) {
    return <Skeleton className="mx-auto h-[260px] w-[260px] rounded-full" />;
  }

  if (data.length === 0) {
    return (
      <p className="text-muted-foreground py-8 text-center text-sm">
        {t('portfolio.empty')}
      </p>
    );
  }

  // Sort by canonical class order so the legend reads predictably.
  const sorted = [...data].sort(
    (a, b) =>
      CLASS_ORDER.indexOf(a.class as AssetClass) -
      CLASS_ORDER.indexOf(b.class as AssetClass),
  );

  const chartData = sorted.map((row) => ({
    key: row.class as AssetClass,
    label: t(`portfolio.class.${row.class}`, row.class),
    value: row.value,
    pct: row.pct,
    fill: CLASS_COLOR[row.class as AssetClass] ?? 'var(--chart-5)',
  }));

  return (
    <div>
      <ChartContainer config={config} className="mx-auto aspect-square h-[260px]">
        <PieChart>
          <ChartTooltip
            content={
              <ChartTooltipContent
                hideLabel
                formatter={(v, name) => {
                  const num = typeof v === 'number' ? v : Number(v);
                  const row = chartData.find((d) => d.key === name);
                  const label = row?.label ?? name;
                  const pct = row?.pct ?? 0;
                  return (
                    <div className="flex w-full items-center justify-between gap-3">
                      <span className="text-muted-foreground">{label}</span>
                      <span className="font-mono tabular-nums">
                        {num.toLocaleString(undefined, {
                          minimumFractionDigits: 0,
                          maximumFractionDigits: 0,
                        })}{' '}
                        {base} ({pct.toFixed(1)}%)
                      </span>
                    </div>
                  );
                }}
              />
            }
          />
          <Pie
            data={chartData}
            dataKey="value"
            nameKey="key"
            innerRadius={55}
            outerRadius={85}
            paddingAngle={2}
            strokeWidth={1}
          >
            {chartData.map((d) => (
              <Cell key={d.key} fill={d.fill} />
            ))}
          </Pie>
          <ChartLegend content={<ChartLegendContent nameKey="key" />} />
        </PieChart>
      </ChartContainer>
      {missing.length > 0 && (
        <p className="text-muted-foreground mt-2 text-center text-xs">
          {t('portfolio.fxMissing', {
            defaultValue:
              'Some positions had no FX rate — counted at native value: {{list}}',
            list: missing.join(', '),
          })}
        </p>
      )}
    </div>
  );
}
