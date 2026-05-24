import { useMemo } from 'react';
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
import type { AssetClass, Position } from '@/lib/api';

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

export function AllocationPie({ positions }: { positions: Position[] }) {
  const { t } = useTranslation();

  const { data, total } = useMemo(() => {
    const byClass = new Map<AssetClass, number>();
    for (const p of positions) {
      if (Number(p.qty) <= 0) continue;
      if (p.market_value === null) continue;
      const mv = Number(p.market_value);
      if (!Number.isFinite(mv) || mv === 0) continue;
      byClass.set(p.asset_class, (byClass.get(p.asset_class) ?? 0) + mv);
    }
    const sum = [...byClass.values()].reduce((a, b) => a + b, 0);
    // Sort by the canonical order, but include only classes present.
    const rows = CLASS_ORDER.filter((c) => byClass.has(c)).map((c) => ({
      key: c,
      label: t(`portfolio.class.${c}`),
      value: byClass.get(c)!,
      fill: CLASS_COLOR[c],
    }));
    return { data: rows, total: sum };
  }, [positions, t]);

  const config = useMemo<ChartConfig>(() => {
    const cfg: ChartConfig = {};
    for (const c of CLASS_ORDER) {
      cfg[c] = { label: t(`portfolio.class.${c}`), color: CLASS_COLOR[c] };
    }
    return cfg;
  }, [t]);

  if (data.length === 0 || total === 0) {
    return (
      <p className="text-muted-foreground py-8 text-center text-sm">
        {t('portfolio.empty')}
      </p>
    );
  }

  return (
    <ChartContainer config={config} className="mx-auto aspect-square h-[260px]">
      <PieChart>
        <ChartTooltip
          content={
            <ChartTooltipContent
              hideLabel
              formatter={(v, name) => {
                const num = typeof v === 'number' ? v : Number(v);
                const pct = total > 0 ? (num / total) * 100 : 0;
                const label = config[name as string]?.label ?? name;
                return (
                  <div className="flex w-full items-center justify-between gap-3">
                    <span className="text-muted-foreground">{label}</span>
                    <span className="font-mono tabular-nums">
                      {num.toLocaleString(undefined, {
                        minimumFractionDigits: 0,
                        maximumFractionDigits: 0,
                      })}{' '}
                      ({pct.toFixed(1)}%)
                    </span>
                  </div>
                );
              }}
            />
          }
        />
        <Pie
          data={data}
          dataKey="value"
          nameKey="key"
          innerRadius={55}
          outerRadius={85}
          paddingAngle={2}
          strokeWidth={1}
        >
          {data.map((d) => (
            <Cell key={d.key} fill={d.fill} />
          ))}
        </Pie>
        <ChartLegend content={<ChartLegendContent nameKey="key" />} />
      </PieChart>
    </ChartContainer>
  );
}
