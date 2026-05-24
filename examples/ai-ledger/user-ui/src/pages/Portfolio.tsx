import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toast } from 'sonner';
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card';
import { Skeleton } from '@/components/ui/skeleton';
import { PositionsList } from '@/components/portfolio/positions-list';
import { TradesList } from '@/components/portfolio/trades-list';
import { AllocationPie } from '@/components/portfolio/allocation-pie';
import {
  ledgerApi,
  type AssetWithPrice,
  type Position,
  type Trade,
} from '@/lib/api';

export function Portfolio() {
  const { t } = useTranslation();
  const [positions, setPositions] = useState<Position[]>([]);
  const [trades, setTrades] = useState<Trade[]>([]);
  const [assets, setAssets] = useState<AssetWithPrice[]>([]);
  const [loading, setLoading] = useState(true);

  const reload = useCallback(async () => {
    setLoading(true);
    try {
      const [pos, tr, as] = await Promise.all([
        ledgerApi.positions(),
        ledgerApi.trades(undefined, 100),
        ledgerApi.assets(),
      ]);
      setPositions(pos.positions ?? []);
      setTrades(tr.trades ?? []);
      setAssets(as.assets ?? []);
    } catch (e) {
      toast.error(`${t('common.error')}: ${(e as Error).message}`);
    } finally {
      setLoading(false);
    }
  }, [t]);

  useEffect(() => {
    reload();
  }, [reload]);

  // asset_id → symbol map for the trades list. Prefer positions (already
  // joined), fall back to /assets so trades for fully-closed positions still
  // get a friendly ticker.
  const symbolByAssetId = useMemo(() => {
    const m = new Map<string, string>();
    for (const a of assets) m.set(a.asset.id, a.asset.symbol);
    for (const p of positions) m.set(p.asset_id, p.symbol);
    return m;
  }, [positions, assets]);

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-2">
        <h1 className="text-2xl font-bold tracking-tight">{t('portfolio.title')}</h1>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('portfolio.positions')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="space-y-2">
              <Skeleton className="h-12 w-full" />
              <Skeleton className="h-12 w-full" />
              <Skeleton className="h-12 w-full" />
            </div>
          ) : (
            <PositionsList positions={positions} />
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('portfolio.allocation')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <Skeleton className="mx-auto h-[260px] w-[260px] rounded-full" />
          ) : (
            <AllocationPie />
          )}
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">{t('portfolio.trades')}</CardTitle>
        </CardHeader>
        <CardContent>
          {loading ? (
            <div className="space-y-2">
              <Skeleton className="h-10 w-full" />
              <Skeleton className="h-10 w-full" />
              <Skeleton className="h-10 w-full" />
            </div>
          ) : (
            <TradesList trades={trades} symbolByAssetId={symbolByAssetId} />
          )}
        </CardContent>
      </Card>
    </div>
  );
}
