import { useEffect } from 'react';
import { useForm, Controller } from 'react-hook-form';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import { Button } from '@/components/ui/button';
import { Label } from '@/components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { ledgerApi, type Account, type Transaction } from '@/lib/api';

interface FormValues {
  kind: 'expense' | 'income' | 'transfer';
  amount: string;
  currency: string;
  account_id: string;
  category: string;
  note: string;
  occurred_at: string; // YYYY-MM-DD
}

function todayIso(): string {
  return new Date().toISOString().slice(0, 10);
}

export function TxnForm({
  open,
  onClose,
  onSaved,
  editing,
  accounts,
}: {
  open: boolean;
  onClose: () => void;
  onSaved: () => void;
  editing: Transaction | null;
  accounts: Account[];
}) {
  const { t } = useTranslation();
  const noAccounts = accounts.length === 0;

  const { register, handleSubmit, control, reset, watch, setValue, formState } =
    useForm<FormValues>({
      defaultValues: {
        kind: 'expense',
        amount: '',
        currency: accounts[0]?.currency ?? 'USD',
        account_id: accounts[0]?.id ?? '',
        category: '',
        note: '',
        occurred_at: todayIso(),
      },
    });

  // Reset when dialog opens (or editing target changes)
  useEffect(() => {
    if (!open) return;
    if (editing) {
      reset({
        kind: editing.kind,
        amount: editing.amount,
        currency: editing.currency,
        account_id: editing.account_id,
        category: editing.category ?? '',
        note: editing.note ?? '',
        occurred_at: editing.occurred_at.slice(0, 10),
      });
    } else {
      reset({
        kind: 'expense',
        amount: '',
        currency: accounts[0]?.currency ?? 'USD',
        account_id: accounts[0]?.id ?? '',
        category: '',
        note: '',
        occurred_at: todayIso(),
      });
    }
  }, [open, editing, accounts, reset]);

  // Auto-update currency when account changes (only for new txns; for edits keep
  // the persisted currency unless the user manually re-picks the account).
  const accountId = watch('account_id');
  useEffect(() => {
    if (!accountId) return;
    const a = accounts.find((x) => x.id === accountId);
    if (a) setValue('currency', a.currency);
  }, [accountId, accounts, setValue]);

  async function onSubmit(v: FormValues) {
    try {
      const body = {
        ...v,
        occurred_at: new Date(v.occurred_at).toISOString(),
        category: v.category || null,
        note: v.note || null,
      };
      if (editing) {
        await ledgerApi.updateTransaction(editing.id, body);
        toast.success(t('ledger.updatedToast'));
      } else {
        await ledgerApi.createTransaction(body);
        toast.success(t('ledger.createdToast'));
      }
      onSaved();
      onClose();
    } catch (e) {
      toast.error(`${t('common.error')}: ${(e as Error).message}`);
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => !o && onClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>{editing ? t('ledger.edit') : t('ledger.new')}</DialogTitle>
          {noAccounts && (
            <DialogDescription className="text-destructive">
              {t('ledger.noAccountsHint')}
            </DialogDescription>
          )}
        </DialogHeader>
        <form onSubmit={handleSubmit(onSubmit)} className="space-y-3">
          <div className="space-y-1.5">
            <Label htmlFor="txn-kind">{t('ledger.kind')}</Label>
            <Controller
              control={control}
              name="kind"
              render={({ field }) => (
                <Select value={field.value} onValueChange={field.onChange}>
                  <SelectTrigger id="txn-kind" className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="expense">{t('ledger.expense')}</SelectItem>
                    <SelectItem value="income">{t('ledger.income')}</SelectItem>
                    <SelectItem value="transfer">{t('ledger.transfer')}</SelectItem>
                  </SelectContent>
                </Select>
              )}
            />
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1.5">
              <Label htmlFor="txn-amount">{t('ledger.amount')}</Label>
              <Input
                id="txn-amount"
                {...register('amount', { required: true })}
                type="text"
                inputMode="decimal"
                placeholder="0.00"
              />
            </div>
            <div className="space-y-1.5">
              <Label htmlFor="txn-currency">{t('ledger.currency')}</Label>
              <Input id="txn-currency" {...register('currency', { required: true })} />
            </div>
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="txn-account">{t('ledger.account')}</Label>
            <Controller
              control={control}
              name="account_id"
              rules={{ required: true }}
              render={({ field }) => (
                <Select
                  value={field.value}
                  onValueChange={field.onChange}
                  disabled={noAccounts}
                >
                  <SelectTrigger id="txn-account" className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {accounts.map((a) => (
                      <SelectItem key={a.id} value={a.id}>
                        {a.name} ({a.currency})
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              )}
            />
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="txn-category">{t('ledger.category')}</Label>
            <Input
              id="txn-category"
              {...register('category')}
              placeholder={t('ledger.categoryPlaceholder')}
            />
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="txn-date">{t('ledger.date')}</Label>
            <Input
              id="txn-date"
              {...register('occurred_at', { required: true })}
              type="date"
            />
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="txn-note">{t('ledger.note')}</Label>
            <Textarea id="txn-note" {...register('note')} rows={2} />
          </div>
          <DialogFooter>
            <Button type="button" variant="ghost" onClick={onClose}>
              {t('ledger.cancel')}
            </Button>
            <Button type="submit" disabled={noAccounts || formState.isSubmitting}>
              {t('ledger.save')}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
