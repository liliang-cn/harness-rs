import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Plus } from 'lucide-react';
import { toast } from 'sonner';
import { Button } from '@/components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { TxnList } from '@/components/transactions/txn-list';
import { TxnForm } from '@/components/transactions/txn-form';
import {
  TxnFilters,
  periodToRange,
  type Period,
} from '@/components/transactions/txn-filters';
import { ledgerApi, type Account, type Transaction } from '@/lib/api';

export function Ledger() {
  const { t } = useTranslation();
  const [txns, setTxns] = useState<Transaction[]>([]);
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [loading, setLoading] = useState(true);
  const [period, setPeriod] = useState<Period>('thisMonth');
  const [formOpen, setFormOpen] = useState(false);
  const [editing, setEditing] = useState<Transaction | null>(null);

  const reload = useCallback(async () => {
    setLoading(true);
    try {
      const [r, a] = await Promise.all([
        // Server currently caps to 365 days and ignores from/to — we still pass
        // a generous `limit` and do the period narrowing client-side below.
        ledgerApi.transactions({ limit: 500 }),
        ledgerApi.accounts(),
      ]);
      setTxns(r.transactions ?? []);
      setAccounts(a.accounts ?? []);
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

  async function onDelete(tx: Transaction) {
    if (!window.confirm(t('ledger.deleteConfirm'))) return;
    try {
      await ledgerApi.deleteTransaction(tx.id);
      toast.success(t('ledger.deletedToast'));
      reload();
    } catch (e) {
      toast.error(`${t('common.error')}: ${(e as Error).message}`);
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-2">
        <h1 className="text-2xl font-bold tracking-tight">{t('ledger.title')}</h1>
        <div className="flex items-center gap-2">
          <TxnFilters period={period} onPeriodChange={setPeriod} />
          <Button
            size="sm"
            onClick={() => {
              setEditing(null);
              setFormOpen(true);
            }}
          >
            <Plus className="size-4" />
            <span className="hidden sm:inline">{t('ledger.new')}</span>
          </Button>
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
            <TxnList
              txns={visibleTxns}
              onEdit={(tx) => {
                setEditing(tx);
                setFormOpen(true);
              }}
              onDelete={onDelete}
            />
          )}
        </CardContent>
      </Card>

      <TxnForm
        open={formOpen}
        onClose={() => setFormOpen(false)}
        onSaved={reload}
        editing={editing}
        accounts={accounts}
      />
    </div>
  );
}
