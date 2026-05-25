import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Download } from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { ModelPicker } from '@/components/profile/model-picker';
import { noteApi, getToken } from '@/lib/api';

export function Profile() {
  const { t } = useTranslation();
  const [user, setUser] = useState<any>(null);
  useEffect(() => { noteApi.me().then((j) => setUser(j.user)).catch(() => {}); }, []);

  async function exportZip() {
    const resp = await fetch('/api/notes/export.zip', { headers: { Authorization: `Bearer ${getToken() ?? ''}` } });
    const blob = await resp.blob();
    const a = document.createElement('a');
    a.href = URL.createObjectURL(blob); a.download = 'notes.zip'; a.click();
    URL.revokeObjectURL(a.href);
  }

  return (
    <div className="space-y-4">
      <h1 className="text-xl font-semibold">{t('nav.profile')}</h1>
      <Card className="space-y-1 p-4">
        <div className="text-sm">{user?.email}</div>
        <div className="text-muted-foreground text-xs">{user?.tier}</div>
      </Card>
      <Card className="p-4">
        <ModelPicker tier={user?.tier ?? 'trial'} current={user?.preferred_model} />
      </Card>
      <Button variant="outline" onClick={exportZip}>
        <Download className="size-4" /> {t('profile.export')}
      </Button>
    </div>
  );
}
