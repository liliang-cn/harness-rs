import { useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { cn } from '@/lib/utils';
import { Button } from '@/components/ui/button';
import { Input } from '@/components/ui/input';
import { Field, FieldGroup, FieldLabel } from '@/components/ui/field';
import { Tabs, TabsList, TabsTrigger, TabsContent } from '@/components/ui/tabs';
import { ledgerApi, setToken } from '@/lib/api';

export function LoginForm({
  className,
  ...props
}: React.ComponentProps<'div'>) {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [mode, setMode] = useState<'login' | 'register'>('login');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');

  // Single submit, mode decides which endpoint to hit so we don't have
  // two near-duplicate forms.
  async function onSubmit(e: React.FormEvent<HTMLFormElement>) {
    e.preventDefault();
    setError('');
    const fd = new FormData(e.currentTarget);
    const email = (fd.get('email') as string)?.trim();
    const password = fd.get('password') as string;
    const invite = (fd.get('invite_code') as string)?.trim();
    setBusy(true);
    try {
      const r =
        mode === 'login'
          ? await ledgerApi.login(email, password)
          : await ledgerApi.register(email, password, invite || undefined);
      setToken(r.token);
      navigate('/');
    } catch (err) {
      setError(String((err as Error).message || err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className={cn('flex flex-col gap-6', className)} {...props}>
      <div className="flex flex-col items-center gap-1 text-center">
        <h1 className="text-2xl font-bold">{t('login.title')}</h1>
        <p className="text-muted-foreground text-sm">{t('login.subtitle')}</p>
      </div>
      <Tabs
        value={mode}
        onValueChange={(v) => {
          setMode(v as 'login' | 'register');
          setError('');
        }}
      >
        <TabsList className="w-full">
          <TabsTrigger value="login" className="flex-1">
            {t('login.submit')}
          </TabsTrigger>
          <TabsTrigger value="register" className="flex-1">
            {t('login.register')}
          </TabsTrigger>
        </TabsList>

        <TabsContent value="login" className="mt-6">
          <form onSubmit={onSubmit}>
            <FieldGroup>
              <Field>
                <FieldLabel htmlFor="email-login">{t('login.email')}</FieldLabel>
                <Input
                  id="email-login"
                  name="email"
                  type="email"
                  autoComplete="email"
                  required
                />
              </Field>
              <Field>
                <FieldLabel htmlFor="password-login">{t('login.password')}</FieldLabel>
                <Input
                  id="password-login"
                  name="password"
                  type="password"
                  autoComplete="current-password"
                  required
                />
              </Field>
              {error && (
                <p className="text-destructive text-sm" role="alert">
                  {error}
                </p>
              )}
              <Button type="submit" disabled={busy}>
                {busy ? '…' : t('login.submit')}
              </Button>
            </FieldGroup>
          </form>
        </TabsContent>

        <TabsContent value="register" className="mt-6">
          <form onSubmit={onSubmit}>
            <FieldGroup>
              <Field>
                <FieldLabel htmlFor="email-reg">{t('login.email')}</FieldLabel>
                <Input
                  id="email-reg"
                  name="email"
                  type="email"
                  autoComplete="email"
                  required
                />
              </Field>
              <Field>
                <FieldLabel htmlFor="password-reg">{t('login.password')}</FieldLabel>
                <Input
                  id="password-reg"
                  name="password"
                  type="password"
                  autoComplete="new-password"
                  required
                />
              </Field>
              <Field>
                <FieldLabel htmlFor="invite">{t('login.invite')}</FieldLabel>
                <Input id="invite" name="invite_code" type="text" />
              </Field>
              {error && (
                <p className="text-destructive text-sm" role="alert">
                  {error}
                </p>
              )}
              <Button type="submit" disabled={busy}>
                {busy ? '…' : t('login.registerSubmit')}
              </Button>
            </FieldGroup>
          </form>
        </TabsContent>
      </Tabs>
    </div>
  );
}
