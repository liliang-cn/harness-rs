import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import {
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
} from '@/components/ui/select';
import { noteApi } from '@/lib/api';

export function ModelPicker({ tier, current }: { tier: string; current?: string }) {
  const { t } = useTranslation();
  const [models, setModels] = useState<string[]>([]);
  const [value, setValue] = useState(current ?? '');
  useEffect(() => { noteApi.info().then((j) => setModels(j.allowed_models)).catch(() => {}); }, []);
  const disabled = tier === 'trial';
  async function pick(m: string) {
    setValue(m);
    try { await noteApi.setModel(m); toast.success('ok'); } catch (e) { toast.error((e as Error).message); }
  }
  return (
    <div className="space-y-1">
      <div className="text-sm font-medium">{t('profile.model')}</div>
      {disabled ? (
        <p className="text-muted-foreground text-xs">{t('profile.modelTrial')}</p>
      ) : (
        <Select value={value} onValueChange={pick}>
          <SelectTrigger className="w-full"><SelectValue placeholder="—" /></SelectTrigger>
          <SelectContent>
            {models.map((m) => <SelectItem key={m} value={m}>{m}</SelectItem>)}
          </SelectContent>
        </Select>
      )}
    </div>
  );
}
