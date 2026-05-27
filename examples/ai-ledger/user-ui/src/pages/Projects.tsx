import { useCallback, useEffect, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { Target, ChevronRight, Sparkles, Search } from 'lucide-react';
import { format, parseISO } from 'date-fns';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Input } from '@/components/ui/input';
import { Skeleton } from '@/components/ui/skeleton';
import { openChatWith } from '@/lib/chat-prefill';
import { ledgerApi, type Project, type NoteSearchHit } from '@/lib/api';

export function Projects() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [projects, setProjects] = useState<Project[] | null>(null);

  // Search state
  const [query, setQuery] = useState('');
  const [searchResults, setSearchResults] = useState<NoteSearchHit[] | null>(null);
  const [searching, setSearching] = useState(false);

  const load = useCallback(() => {
    setProjects(null);
    ledgerApi.projects('all').then((j) => setProjects(j.projects)).catch(() => setProjects([]));
  }, []);
  useEffect(load, [load]);

  const now = Date.now();
  const active = (projects ?? []).filter((p) => p.status === 'active' && !p.parent_id);
  const due = active.filter((p) => p.next_review_at && Date.parse(p.next_review_at) <= now);

  async function doSearch(q: string) {
    if (!q.trim()) {
      setSearchResults(null);
      return;
    }
    setSearching(true);
    try {
      const res = await ledgerApi.searchNotes(q.trim());
      setSearchResults(res.hits);
    } catch {
      setSearchResults([]);
    } finally {
      setSearching(false);
    }
  }

  function onSearchChange(e: React.ChangeEvent<HTMLInputElement>) {
    const val = e.target.value;
    setQuery(val);
    if (!val.trim()) {
      setSearchResults(null);
    }
  }

  function onSearchKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    if (e.key === 'Enter') doSearch(query);
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <h1 className="text-xl font-semibold">{t('project.title')}</h1>
        <Button variant="outline" onClick={() => openChatWith('我想开一个新项目：')}>
          <Sparkles className="size-4" /> {t('project.addProject')}
        </Button>
      </div>

      {/* Semantic search box */}
      <div className="flex gap-2">
        <div className="relative flex-1">
          <Search className="text-muted-foreground absolute top-2.5 left-2.5 size-4" />
          <Input
            className="pl-8"
            placeholder={t('project.searchPlaceholder')}
            value={query}
            onChange={onSearchChange}
            onKeyDown={onSearchKeyDown}
          />
        </div>
        <Button variant="outline" onClick={() => doSearch(query)} disabled={searching}>
          {searching ? t('common.loading') : t('project.search')}
        </Button>
      </div>

      {/* Search results */}
      {searchResults !== null && (
        <section className="space-y-2">
          <h2 className="text-muted-foreground text-xs font-medium uppercase">
            {t('project.searchResults')} ({searchResults.length})
          </h2>
          {searchResults.length === 0 ? (
            <p className="text-muted-foreground text-sm">{t('notes.empty')}</p>
          ) : (
            searchResults.map((hit) => (
              <Card
                key={hit.id}
                onClick={() => navigate(`/app/notes/${hit.id}`)}
                className="hover:bg-accent flex flex-row cursor-pointer items-center gap-2 p-3"
              >
                <div className="min-w-0 flex-1">
                  <div className="truncate text-sm font-medium">
                    {hit.title?.trim() || hit.body.slice(0, 50)}
                  </div>
                  {hit.tags.length > 0 && (
                    <div className="text-muted-foreground mt-0.5 text-xs">
                      {hit.tags.join(', ')}
                    </div>
                  )}
                </div>
                <ChevronRight className="text-muted-foreground size-4 shrink-0" />
              </Card>
            ))
          )}
        </section>
      )}

      {/* Project list */}
      {projects === null ? (
        <div className="space-y-2">
          <Skeleton className="h-16 w-full" />
          <Skeleton className="h-16 w-full" />
        </div>
      ) : active.length === 0 ? (
        <p className="text-muted-foreground py-12 text-center text-sm">{t('project.empty')}</p>
      ) : (
        <>
          <section className="space-y-2">
            <h2 className="text-muted-foreground text-xs font-medium uppercase">{t('project.due')}</h2>
            {due.length === 0 ? (
              <p className="text-muted-foreground text-sm">{t('project.noDue')}</p>
            ) : (
              due.map((p) => (
                <Card key={p.id} className="flex flex-row items-center justify-between gap-2 p-3">
                  <button
                    className="min-w-0 flex-1 text-left"
                    onClick={() => navigate(`/app/projects/${p.id}`)}
                  >
                    <div className="truncate text-sm font-medium">{p.name}</div>
                  </button>
                  <Button size="sm" onClick={() => openChatWith(`复盘：${p.name}`)}>
                    {t('project.review')}
                  </Button>
                </Card>
              ))
            )}
          </section>

          <section className="space-y-2">
            <h2 className="text-muted-foreground text-xs font-medium uppercase">{t('project.projects')}</h2>
            {active.map((p) => (
              <Card
                key={p.id}
                onClick={() => navigate(`/app/projects/${p.id}`)}
                className="hover:bg-accent flex flex-row cursor-pointer items-center gap-2 p-3"
              >
                <Target className="text-muted-foreground size-4 shrink-0" />
                <div className="min-w-0 flex-1">
                  <div className="truncate text-sm font-medium">{p.name}</div>
                  {p.target_date && (
                    <div className="text-muted-foreground text-xs">
                      {t('project.targetDate')}: {format(parseISO(p.target_date), 'yyyy-MM-dd')}
                    </div>
                  )}
                </div>
                <ChevronRight className="text-muted-foreground size-4" />
              </Card>
            ))}
          </section>
        </>
      )}
    </div>
  );
}
