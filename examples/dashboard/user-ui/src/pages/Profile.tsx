import { useCallback, useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { KeyRound, Brain } from 'lucide-react';
import { toast } from 'sonner';
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from '@/components/ui/card';
import { Button } from '@/components/ui/button';
import { Skeleton } from '@/components/ui/skeleton';
import { ledgerApi, type User } from '@/lib/api';
import { AccountCard } from '@/components/profile/account-card';
import { PasswordForm } from '@/components/profile/password-form';
import { ModelPicker } from '@/components/profile/model-picker';
import { DigestCard } from '@/components/profile/digest-card';
import { MemorySheet } from '@/components/memory/memory-sheet';

/**
 * Profile page — stack of cards, mobile-first.
 *   1. Account (read-only key-values)
 *   2. Password (dialog)
 *   3. Model picker (disabled for trial)
 *   4. Memory (sheet)
 *
 * `me` is loaded once on mount + refetched after the model picker writes
 * (so we always reflect server-side preferred_model). Memory count is
 * fetched lazily and refreshed every time the sheet mutates state.
 */
export function Profile() {
  const { t } = useTranslation();
  const [user, setUser] = useState<User | null>(null);
  const [effectiveModel, setEffectiveModel] = useState<string | undefined>();
  const [loading, setLoading] = useState(true);
  const [pwOpen, setPwOpen] = useState(false);
  const [memOpen, setMemOpen] = useState(false);
  const [memCount, setMemCount] = useState<number | null>(null);

  const loadMe = useCallback(async () => {
    try {
      const r = await ledgerApi.me();
      setUser(r.user);
      setEffectiveModel(r.effective_model_id);
    } catch (e) {
      toast.error(`${t('common.error')}: ${(e as Error).message}`);
    } finally {
      setLoading(false);
    }
  }, [t]);

  const loadMemCount = useCallback(async () => {
    try {
      const r = await ledgerApi.memories();
      setMemCount(r.count);
    } catch {
      // Memory count is best-effort; skip toast spam if the file is just
      // missing (server still returns {count:0,memories:[]}).
      setMemCount(null);
    }
  }, []);

  useEffect(() => {
    loadMe();
    loadMemCount();
  }, [loadMe, loadMemCount]);

  if (loading || !user) {
    return (
      <div className="space-y-4">
        <Skeleton className="h-44 w-full" />
        <Skeleton className="h-28 w-full" />
        <Skeleton className="h-28 w-full" />
        <Skeleton className="h-28 w-full" />
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <h1 className="text-xl font-semibold md:text-2xl">{t('profile.title')}</h1>

      <AccountCard user={user} />

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('profile.password')}</CardTitle>
        </CardHeader>
        <CardContent>
          <Button variant="outline" onClick={() => setPwOpen(true)}>
            <KeyRound className="size-4" />
            {t('profile.changePassword')}
          </Button>
        </CardContent>
      </Card>

      <ModelPicker
        user={user}
        effectiveModelId={effectiveModel}
        onChanged={loadMe}
      />

      <DigestCard />

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('profile.memory')}</CardTitle>
        </CardHeader>
        <CardContent>
          <Button variant="outline" onClick={() => setMemOpen(true)}>
            <Brain className="size-4" />
            {t('profile.memoryOpen', { count: memCount ?? 0 })}
          </Button>
        </CardContent>
      </Card>

      <PasswordForm open={pwOpen} onOpenChange={setPwOpen} />
      <MemorySheet
        open={memOpen}
        onOpenChange={setMemOpen}
        onChanged={loadMemCount}
      />
    </div>
  );
}
