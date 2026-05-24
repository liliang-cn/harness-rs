import { useTranslation } from 'react-i18next';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Badge } from '@/components/ui/badge';
import type { User } from '@/lib/api';

interface AccountCardProps {
  user: User;
}

/**
 * Read-only key-value summary of the signed-in account. All fields are
 * derived from /api/me — no mutations from this card. Mutation lives in the
 * other profile sub-cards (password, model, etc).
 */
export function AccountCard({ user }: AccountCardProps) {
  const { t, i18n } = useTranslation();
  const joined = user.created_at
    ? new Date(user.created_at).toLocaleDateString(i18n.language, {
        year: 'numeric',
        month: 'short',
        day: 'numeric',
      })
    : '—';
  const rows: Array<[string, React.ReactNode]> = [
    [t('profile.email'), user.email],
    [
      t('profile.tier'),
      <Badge key="tier" variant="secondary" className="capitalize">
        {user.tier}
      </Badge>,
    ],
    [t('profile.joined'), joined],
    [
      t('profile.userId'),
      <span key="id" className="font-mono text-xs">
        {user.id}
      </span>,
    ],
    [t('profile.baseCurrency'), user.base_currency],
  ];
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{t('profile.account')}</CardTitle>
      </CardHeader>
      <CardContent>
        <dl className="divide-border divide-y text-sm">
          {rows.map(([k, v]) => (
            <div
              key={k}
              className="flex items-center justify-between gap-3 py-2 first:pt-0 last:pb-0"
            >
              <dt className="text-muted-foreground">{k}</dt>
              <dd className="min-w-0 truncate text-right">{v}</dd>
            </div>
          ))}
        </dl>
      </CardContent>
    </Card>
  );
}
