import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { ledgerApi, type ModelOption, type User } from '@/lib/api';
import { Skeleton } from '@/components/ui/skeleton';

interface ModelPickerProps {
  user: User;
  effectiveModelId?: string;
  onChanged?: () => void;
}

// Sentinel for "no preferred model — use the server default". The Select
// can't accept an empty string as a value, so we keep this private to the
// component and translate to/from `null` at the API edge.
const DEFAULT_VALUE = '__default__';

/**
 * Model preference picker. Trial users see a disabled select + hint
 * ("Upgrade to paid to switch models"). Paid+ users get the live list from
 * /api/info filtered to only `available: true` entries, plus a "Server
 * default" option that clears the preference (POSTs `{model: null}`).
 */
export function ModelPicker({
  user,
  effectiveModelId,
  onChanged,
}: ModelPickerProps) {
  const { t } = useTranslation();
  const [models, setModels] = useState<ModelOption[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);

  const trial = user.tier === 'trial';

  useEffect(() => {
    let cancelled = false;
    ledgerApi
      .info()
      .then((info) => {
        if (cancelled) return;
        setModels(info.available_models);
      })
      .catch((e) => {
        if (!cancelled) toast.error(`${t('common.error')}: ${(e as Error).message}`);
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [t]);

  async function handleChange(v: string) {
    const next = v === DEFAULT_VALUE ? null : v;
    setSaving(true);
    try {
      await ledgerApi.setModel(next);
      onChanged?.();
    } catch (e) {
      toast.error(`${t('common.error')}: ${(e as Error).message}`);
    } finally {
      setSaving(false);
    }
  }

  const currentValue = user.preferred_model ?? DEFAULT_VALUE;

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">{t('profile.model')}</CardTitle>
        {trial ? (
          <CardDescription>{t('profile.modelTrialHint')}</CardDescription>
        ) : effectiveModelId ? (
          <CardDescription className="font-mono text-xs">
            {effectiveModelId}
          </CardDescription>
        ) : null}
      </CardHeader>
      <CardContent>
        {loading ? (
          <Skeleton className="h-9 w-full" />
        ) : (
          <Select
            value={currentValue}
            onValueChange={handleChange}
            disabled={trial || saving}
          >
            <SelectTrigger className="w-full">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={DEFAULT_VALUE}>
                {t('profile.modelDefault')}
              </SelectItem>
              {(models ?? [])
                .filter((m) => m.available)
                .map((m) => (
                  <SelectItem key={m.id} value={m.id}>
                    {m.label}{' '}
                    <span className="text-muted-foreground">({m.provider})</span>
                  </SelectItem>
                ))}
            </SelectContent>
          </Select>
        )}
      </CardContent>
    </Card>
  );
}
