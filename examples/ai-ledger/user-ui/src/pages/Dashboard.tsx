import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  Card,
  Typography,
  Space,
  Select,
  Button,
  Spin,
  Tag,
  message,
} from 'antd';
import { ReloadOutlined, RiseOutlined, FallOutlined } from '@ant-design/icons';
import { LineChart, Line, ResponsiveContainer, Tooltip, XAxis, YAxis } from 'recharts';
import { ledgerApi, type NetWorthSnapshot, type Account } from '@/lib/api';

const { Title, Text } = Typography;

const TRACKED_CURRENCIES = [
  'USD', 'EUR', 'GBP', 'JPY', 'CNY', 'HKD', 'SGD', 'AUD', 'CAD', 'CHF', 'KRW',
];

const CURRENCY_SYMBOL: Record<string, string> = {
  USD: '$', EUR: '€', GBP: '£', JPY: '¥', CNY: '¥',
  HKD: 'HK$', SGD: 'S$', AUD: 'A$', CAD: 'C$', CHF: 'CHF ', KRW: '₩',
};

function formatMoney(amt: number, ccy: string): string {
  // JPY / KRW / CNY: 0 decimals; everything else 2.
  const noDecimals = ccy === 'JPY' || ccy === 'KRW';
  return (
    (CURRENCY_SYMBOL[ccy] ?? `${ccy} `) +
    amt.toLocaleString(undefined, {
      minimumFractionDigits: noDecimals ? 0 : 2,
      maximumFractionDigits: noDecimals ? 0 : 2,
    })
  );
}

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
      message.error(`${t('common.error')}: ${(e as Error).message}`);
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
      // Re-pull series in the new base currency for chart continuity
      const s = await ledgerApi.netWorthSeries();
      setSeries(s.series);
    } catch (e) {
      message.error(`${t('common.error')}: ${(e as Error).message}`);
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
      message.error(`${t('common.error')}: ${(e as Error).message}`);
    } finally {
      setRefreshing(false);
    }
  }

  if (loading || !snap) {
    return (
      <div style={{ display: 'flex', justifyContent: 'center', padding: 64 }}>
        <Spin size="large" />
      </div>
    );
  }

  const ccy = snap.base_currency;

  // 30d delta — find the snapshot ~30 days ago in series (closest by date).
  let delta30Pct: number | null = null;
  let delta30Abs: number | null = null;
  if (series.length > 1) {
    const today = new Date(snap.snapshot_date);
    const target = new Date(today);
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
    { label: t('dashboard.cash'), value: snap.cash_amt, color: '#52c41a' },
    { label: t('dashboard.investments'), value: snap.investments_amt, color: '#1677ff' },
    { label: t('dashboard.debt'), value: -snap.debt_amt, color: '#ff4d4f' },
  ];
  const totalAbs =
    Math.abs(snap.cash_amt) + Math.abs(snap.investments_amt) + Math.abs(snap.debt_amt);

  const chartData = series.map((s) => ({
    date: s.snapshot_date,
    net: Number(s.net_amt.toFixed(2)),
  }));

  return (
    <Space direction="vertical" size={24} style={{ width: '100%' }}>
      <Card>
        <Space direction="vertical" size={4} style={{ width: '100%' }}>
          <Space style={{ width: '100%', justifyContent: 'space-between', flexWrap: 'wrap' }}>
            <Title level={5} style={{ margin: 0, color: 'var(--ant-color-text-secondary)', fontWeight: 500 }}>
              {t('dashboard.title')}
            </Title>
            <Space>
              <Select
                size="small"
                value={ccy}
                style={{ width: 110 }}
                onChange={changeCurrency}
                options={TRACKED_CURRENCIES.map((c) => ({
                  value: c,
                  label: `${c} ${CURRENCY_SYMBOL[c] ?? ''}`,
                }))}
              />
              <Button
                size="small"
                icon={<ReloadOutlined />}
                onClick={refreshNow}
                loading={refreshing}
                title={t('dashboard.refreshNow')}
              />
            </Space>
          </Space>
          <Title level={1} style={{ margin: 0, fontVariantNumeric: 'tabular-nums' }}>
            {formatMoney(snap.net_amt, ccy)}
          </Title>
          {delta30Pct !== null && delta30Abs !== null ? (
            <Text type={up ? 'success' : 'danger'} style={{ fontSize: 14 }}>
              {up ? <RiseOutlined /> : <FallOutlined />}{' '}
              {t('dashboard.delta30', {
                value: `${up ? '+' : ''}${formatMoney(delta30Abs, ccy)}`,
                pct: `${up ? '+' : ''}${delta30Pct.toFixed(2)}%`,
              })}
            </Text>
          ) : (
            <Text type="secondary" style={{ fontSize: 13 }}>
              {t('dashboard.noHistory')}
            </Text>
          )}
          <Text type="secondary" style={{ fontSize: 12 }}>
            {t('dashboard.asOf', {
              date: new Date(snap.snapshot_date).toLocaleDateString(i18n.language),
            })}
          </Text>
        </Space>
      </Card>

      <Card title={t('dashboard.composition')}>
        <Space direction="vertical" size={12} style={{ width: '100%' }}>
          {composition.map((row) => {
            const pct = totalAbs > 0 ? (Math.abs(row.value) / totalAbs) * 100 : 0;
            return (
              <div key={row.label}>
                <div style={{ display: 'flex', justifyContent: 'space-between', marginBottom: 4 }}>
                  <Text>
                    <span
                      style={{
                        display: 'inline-block',
                        width: 8,
                        height: 8,
                        borderRadius: 4,
                        background: row.color,
                        marginRight: 8,
                      }}
                    />
                    {row.label}
                  </Text>
                  <Text style={{ fontVariantNumeric: 'tabular-nums' }}>
                    {formatMoney(row.value, ccy)}{' '}
                    <Text type="secondary" style={{ fontSize: 12 }}>
                      ({pct.toFixed(1)}%)
                    </Text>
                  </Text>
                </div>
                <div
                  style={{
                    height: 6,
                    borderRadius: 3,
                    background: 'var(--ant-color-fill-secondary)',
                    overflow: 'hidden',
                  }}
                >
                  <div
                    style={{
                      width: `${pct}%`,
                      height: '100%',
                      background: row.color,
                      transition: 'width 0.3s',
                    }}
                  />
                </div>
              </div>
            );
          })}
        </Space>
      </Card>

      {chartData.length > 1 && (
        <Card title={t('dashboard.trend12mo')}>
          <ResponsiveContainer width="100%" height={220}>
            <LineChart data={chartData}>
              <XAxis
                dataKey="date"
                tickFormatter={(d) =>
                  new Date(d).toLocaleDateString(i18n.language, {
                    month: 'short',
                    day: 'numeric',
                  })
                }
                tick={{ fontSize: 11 }}
              />
              <YAxis
                domain={['auto', 'auto']}
                tick={{ fontSize: 11 }}
                tickFormatter={(v) => {
                  if (v >= 1e6) return `${(v / 1e6).toFixed(1)}M`;
                  if (v >= 1e3) return `${(v / 1e3).toFixed(0)}k`;
                  return String(v);
                }}
                width={48}
              />
              <Tooltip
                formatter={(v) => (typeof v === 'number' ? formatMoney(v, ccy) : String(v))}
              />
              <Line
                type="monotone"
                dataKey="net"
                stroke="var(--ant-color-primary)"
                strokeWidth={2}
                dot={false}
              />
            </LineChart>
          </ResponsiveContainer>
        </Card>
      )}

      <Card title={`${t('dashboard.accounts')} (${accounts.length})`}>
        {accounts.length === 0 ? (
          <Text type="secondary">{t('dashboard.noAccounts')}</Text>
        ) : (
          <Space direction="vertical" size={6} style={{ width: '100%' }}>
            {accounts.map((a) => (
              <div
                key={a.id}
                style={{
                  display: 'flex',
                  justifyContent: 'space-between',
                  padding: '8px 0',
                  borderBottom: '1px solid var(--ant-color-border-secondary)',
                }}
              >
                <Space>
                  <Text strong>{a.name}</Text>
                  <Tag>{a.kind}</Tag>
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    {a.currency}
                  </Text>
                </Space>
                <Text style={{ fontVariantNumeric: 'tabular-nums' }}>
                  {Number(a.opening_balance).toLocaleString()}
                </Text>
              </div>
            ))}
          </Space>
        )}
      </Card>
    </Space>
  );
}
