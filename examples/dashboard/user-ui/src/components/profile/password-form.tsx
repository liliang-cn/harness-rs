import { useState } from 'react';
import { useForm } from 'react-hook-form';
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
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Field, FieldGroup, FieldLabel } from '@/components/ui/field';
import { ledgerApi } from '@/lib/api';

interface PasswordFormProps {
  open: boolean;
  onOpenChange: (v: boolean) => void;
}

interface FormValues {
  old_password: string;
  new_password: string;
}

/**
 * Change-password dialog. POST /api/me/password kicks every other session out;
 * we surface the count via the success toast so the user knows we just nuked
 * their other devices. The current token stays valid — no need to bounce
 * to /login.
 */
export function PasswordForm({ open, onOpenChange }: PasswordFormProps) {
  const { t } = useTranslation();
  const [submitting, setSubmitting] = useState(false);
  const {
    register,
    handleSubmit,
    reset,
    formState: { errors },
    setError,
    watch,
  } = useForm<FormValues>({
    defaultValues: { old_password: '', new_password: '' },
  });

  async function onSubmit(values: FormValues) {
    if (values.new_password.length < 6) {
      setError('new_password', { message: t('profile.errMinLen') });
      return;
    }
    if (values.new_password === values.old_password) {
      setError('new_password', { message: t('profile.errSame') });
      return;
    }
    setSubmitting(true);
    try {
      const r = await ledgerApi.changePassword(
        values.old_password,
        values.new_password,
      );
      toast.success(
        t('profile.passwordChanged', { count: r.other_sessions_dropped }),
      );
      reset();
      onOpenChange(false);
    } catch (e) {
      toast.error(`${t('common.error')}: ${(e as Error).message}`);
    } finally {
      setSubmitting(false);
    }
  }

  // react-hook-form's `watch` lets us disable the submit button before the
  // user has typed anything — clearer affordance than relying on `required`
  // alone (which only fires on submit attempt).
  const watched = watch();
  const canSubmit =
    !!watched.old_password &&
    !!watched.new_password &&
    !submitting;

  return (
    <Dialog
      open={open}
      onOpenChange={(v) => {
        if (!v) reset();
        onOpenChange(v);
      }}
    >
      <DialogContent>
        <DialogHeader>
          <DialogTitle>{t('profile.changePassword')}</DialogTitle>
          <DialogDescription>{t('profile.password')}</DialogDescription>
        </DialogHeader>
        <form onSubmit={handleSubmit(onSubmit)}>
          <FieldGroup>
            <Field>
              <FieldLabel htmlFor="pw-old">{t('profile.old')}</FieldLabel>
              <Input
                id="pw-old"
                type="password"
                autoComplete="current-password"
                {...register('old_password', { required: true })}
              />
            </Field>
            <Field>
              <FieldLabel htmlFor="pw-new">{t('profile.new')}</FieldLabel>
              <Input
                id="pw-new"
                type="password"
                autoComplete="new-password"
                {...register('new_password', { required: true, minLength: 6 })}
              />
              {errors.new_password?.message && (
                <p className="text-destructive text-xs" role="alert">
                  {errors.new_password.message as string}
                </p>
              )}
            </Field>
          </FieldGroup>
          <DialogFooter className="mt-6">
            <Button
              type="button"
              variant="outline"
              onClick={() => onOpenChange(false)}
              disabled={submitting}
            >
              {t('profile.cancel')}
            </Button>
            <Button type="submit" disabled={!canSubmit}>
              {submitting ? '…' : t('profile.save')}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
