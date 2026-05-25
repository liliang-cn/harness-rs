import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Card } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { useSpace } from '@/components/space-context';
import { noteApi, type SearchHit } from '@/lib/api';

export function Search() {
  const { t } = useTranslation();
  const { space } = useSpace();
  const [q, setQ] = useState('');
  const [hits, setHits] = useState<SearchHit[] | null>(null);

  async function run(query: string) {
    if (!query.trim()) { setHits(null); return; }
    try { const j = await noteApi.search(space, query); setHits(j.hits); } catch { setHits([]); }
  }

  return (
    <div className="space-y-4">
      <Input
        placeholder={t('search.placeholder')} value={q}
        onChange={(e) => setQ(e.target.value)}
        onKeyDown={(e) => { if (e.key === 'Enter') run(q); }}
      />
      {hits === null ? null : hits.length === 0 ? (
        <p className="text-muted-foreground py-12 text-center text-sm">{t('search.empty')}</p>
      ) : (
        <div className="space-y-2">
          {hits.map((h) => (
            <Card key={h.id} className="p-3">
              <div className="flex items-center justify-between">
                <div className="truncate text-sm font-medium">{h.title?.trim() || h.body.slice(0, 40)}</div>
                <span className="text-muted-foreground text-[11px]">
                  {h.via_grep ? t('search.grep') : `${(h.score * 100).toFixed(0)}%`}
                </span>
              </div>
              <div className="text-muted-foreground mt-1 line-clamp-3 text-xs">{h.body}</div>
            </Card>
          ))}
        </div>
      )}
    </div>
  );
}
