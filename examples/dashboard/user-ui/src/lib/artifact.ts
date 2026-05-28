import { ledgerApi } from '@/lib/api';

/** A page the AI asked us to render. Mirrors the render_artifact tool args. */
export interface ArtifactSpec {
  title: string;
  data: { source: string; id?: string };
  code: string;
}

/** Narrow an unknown (from SSE / persisted JSON) into an ArtifactSpec. */
export function asArtifactSpec(v: unknown): ArtifactSpec | null {
  if (!v || typeof v !== 'object') return null;
  const o = v as Record<string, unknown>;
  const data = o.data as Record<string, unknown> | undefined;
  if (
    typeof o.title === 'string' &&
    typeof o.code === 'string' &&
    data &&
    typeof data.source === 'string' &&
    (data.id === undefined || typeof data.id === 'string')
  ) {
    return {
      title: o.title,
      code: o.code,
      data: { source: data.source, id: typeof data.id === 'string' ? data.id : undefined },
    };
  }
  return null;
}

/** Fetch the data a spec binds to. Host-side (uses the user's token); the
 *  result is postMessage'd into the sandbox as window.DATA. Extend this
 *  registry to add sources (e.g. a macro source for the investor bot). */
export async function fetchArtifactData(spec: ArtifactSpec): Promise<unknown> {
  switch (spec.data.source) {
    case 'project':
      if (!spec.data.id) throw new Error('project artifact missing id');
      return await ledgerApi.project(spec.data.id);
    case 'portfolio': {
      const [pos, assets, trades, summary, allocation, nw, nwSeries] = await Promise.all([
        ledgerApi.positions(),
        ledgerApi.assets(),
        ledgerApi.trades(undefined, 200),
        ledgerApi.summary(),
        ledgerApi.allocation(),
        ledgerApi.netWorth(),
        ledgerApi.netWorthSeries(),
      ]);
      return {
        positions: pos.positions,
        assets: assets.assets,
        trades: trades.trades,
        summary,
        allocation,
        netWorth: nw.snapshot,
        netWorthSeries: nwSeries.series,
      };
    }
    default:
      throw new Error(`unknown artifact data source: ${spec.data.source}`);
  }
}
