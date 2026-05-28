import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { ledgerApi, type ModelOption } from '@/lib/api';

/** Compact per-conversation model picker for the chat header. Paid/admin users
 *  switch the model for this chat; trial users see it disabled. `value` is the
 *  session's model id (null → the server default is shown). */
export function ChatModelSelect({
  value,
  onChange,
}: {
  value: string | null;
  onChange: (id: string) => void;
}) {
  const { t } = useTranslation();
  const [models, setModels] = useState<ModelOption[]>([]);
  const [defaultModel, setDefaultModel] = useState('');
  const [trial, setTrial] = useState(false);

  useEffect(() => {
    let cancelled = false;
    ledgerApi
      .info()
      .then((i) => {
        if (cancelled) return;
        setModels(i.available_models.filter((m) => m.available));
        setDefaultModel(i.default_model_id);
      })
      .catch(() => {});
    ledgerApi
      .me()
      .then((j) => {
        if (!cancelled) setTrial((j.user?.tier ?? '') === 'trial');
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  if (models.length === 0) return null;
  const current = value ?? defaultModel;

  return (
    <Select value={current} onValueChange={onChange} disabled={trial}>
      <SelectTrigger
        className="h-7 w-auto gap-1 border-0 bg-transparent px-2 text-xs shadow-none focus:ring-0"
        aria-label={t('chat.model', { defaultValue: 'Model' })}
        title={trial ? t('profile.modelTrialHint') : t('chat.model', { defaultValue: 'Model' })}
      >
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {models.map((m) => (
          <SelectItem key={m.id} value={m.id} className="text-xs">
            {m.label}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}
