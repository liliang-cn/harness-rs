import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { Button } from '@/components/ui/button';
import { TxnList } from '@/components/transactions/txn-list';
import {
  TxnFilters,
  periodToRange,
  type Period,
} from '@/components/transactions/txn-filters';
import { BudgetsList } from '@/components/budgets/budgets-list';
import { SubsList } from '@/components/subscriptions/subs-list';
import { LoansList } from '@/components/loans/loans-list';
import {
  ledgerApi,
  type BudgetStatus,
  type CsvExportKind,
  type Loan,
  type ReportRow,
  type Subscription,
  type Transaction,
} from '@/lib/api';

export function Ledger() {
  const { t } = useTranslation();
  const [txns, setTxns] = useState<Transaction[]>([]);
  const [budgets, setBudgets] = useState<BudgetStatus[]>([]);
  const [subs, setSubs] = useState<Subscription[]>([]);
  const [loans, setLoans] = useState<Loan[] | null>(null);
  const [report, setReport] = useState<ReportRow[]>([]);
  const [loading, setLoading] = useState(true);
  const [period, setPeriod] = useState<Period>('thisMonth');
  const [cancellingId, setCancellingId] = useState<string | null>(null);
  const [exporting, setExporting] = useState<CsvExportKind | null>(null);

  const reload = useCallback(async () => {
    setLoading(true);
    try {
      // Server currently caps to 365 days and ignores from/to — we still pass
      // a generous `limit` and do the period narrowing client-side below.
      const [txnRes, budgetRes, subRes, reportRes, loansRes] = await Promise.all([
        ledgerApi.transactions({ limit: 500 }),
        ledgerApi.budgets(),
        ledgerApi.subscriptions(),
        ledgerApi.monthlyReport(),
        ledgerApi.loans(),
      ]);
      setTxns(txnRes.transactions ?? []);
      setBudgets(budgetRes.budgets ?? []);
      setSubs(subRes.subscriptions ?? []);
      setReport(reportRes.by_category ?? []);
      setLoans(loansRes.loans ?? []);
    } catch (e) {
      toast.error(`${t('common.error')}: ${(e as Error).message}`);
    } finally {
      setLoading(false);
    }
  }, [t]);

  useEffect(() => {
    reload();
  }, [reload]);

  // Client-side period narrowing — backend doesn't honour from/to yet.
  const visibleTxns = useMemo(() => {
    const { from } = periodToRange(period);
    if (!from) return txns;
    const fromTs = new Date(from + 'T00:00:00').getTime();
    return txns.filter((tx) => new Date(tx.occurred_at).getTime() >= fromTs);
  }, [txns, period]);

  const reloadSubs = useCallback(async () => {
    try {
      const r = await ledgerApi.subscriptions();
      setSubs(r.subscriptions ?? []);
    } catch (e) {
      toast.error(`${t('common.error')}: ${(e as Error).message}`);
    }
  }, [t]);

  const onCancelSub = useCallback(
    async (s: Subscription) => {
      const ok = window.confirm(t('subs.cancelConfirm', { name: s.name }));
      if (!ok) return;
      setCancellingId(s.id);
      try {
        await ledgerApi.cancelSubscription(s.id);
        toast.success(t('subs.cancelled', { name: s.name }));
        await reloadSubs();
      } catch (e) {
        toast.error(`${t('common.error')}: ${(e as Error).message}`);
      } finally {
        setCancellingId(null);
      }
    },
    [reloadSubs, t],
  );

  const onExport = useCallback(
    async (kind: CsvExportKind) => {
      setExporting(kind);
      try {
        await ledgerApi.exportCsv(kind);
      } catch (e) {
        toast.error(`${t('common.error')}: ${(e as Error).message}`);
      } finally {
        setExporting(null);
      }
    },
    [t],
  );

  const exportButtons: { kind: CsvExportKind; labelKey: string }[] = [
    { kind: 'transactions', labelKey: 'export.transactions' },
    { kind: 'trades', labelKey: 'export.trades' },
    { kind: 'subscriptions', labelKey: 'export.subscriptions' },
  ];

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-2">
        <h1 className="text-2xl font-bold tracking-tight">{t('ledger.title')}</h1>
        <div className="flex items-center gap-2">
          <TxnFilters period={period} onPeriodChange={setPeriod} />
        </div>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('ledger.transactions')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="space-y-2">
              <Skeleton className="h-10 w-full" />
              <Skeleton className="h-10 w-full" />
              <Skeleton className="h-10 w-full" />
            </div>
          ) : (
            <TxnList txns={visibleTxns} />
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('budgets.title')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="space-y-2">
              <Skeleton className="h-6 w-full" />
              <Skeleton className="h-6 w-full" />
            </div>
          ) : (
            <BudgetsList budgets={budgets} />
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('subs.title')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="space-y-2">
              <Skeleton className="h-10 w-full" />
              <Skeleton className="h-10 w-full" />
            </div>
          ) : (
            <SubsList subs={subs} onCancel={onCancelSub} cancellingId={cancellingId} />
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('loans.title')}</CardTitle>
        </CardHeader>
        <CardContent>
          <LoansList loans={loading ? null : loans} />
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('report.title')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="space-y-2">
              <Skeleton className="h-8 w-full" />
              <Skeleton className="h-8 w-full" />
            </div>
          ) : report.length === 0 ? (
            <p className="text-muted-foreground py-6 text-center text-sm">
              {t('report.empty')}
            </p>
          ) : (
            <ul className="divide-border divide-y">
              {report.map((r) => (
                <li
                  key={`${r.category}-${r.currency}`}
                  className="flex items-center justify-between gap-2 py-2 first:pt-0 last:pb-0"
                >
                  <div className="min-w-0 flex-1">
                    <span className="text-sm font-medium">{r.category}</span>
                    <span className="text-muted-foreground ml-2 text-xs">
                      {t('report.count', { count: r.count })}
                    </span>
                  </div>
                  <span className="text-sm tabular-nums">
                    {r.total} {r.currency}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('export.title')}</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="flex flex-wrap gap-2">
            {exportButtons.map((b) => (
              <Button
                key={b.kind}
                variant="outline"
                size="sm"
                onClick={() => onExport(b.kind)}
                disabled={exporting !== null}
              >
                {exporting === b.kind ? t('common.loading') : t(b.labelKey)}
              </Button>
            ))}
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
