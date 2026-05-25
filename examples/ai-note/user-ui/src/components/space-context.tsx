import { createContext, useContext, useState, type ReactNode } from 'react';
import type { Space } from '@/lib/api';

const KEY = 'ai-note-space';
function initial(): Space {
  const v = localStorage.getItem(KEY);
  return v === 'work' || v === 'life' ? v : 'life';
}

interface Ctx { space: Space; setSpace: (s: Space) => void; }
const SpaceCtx = createContext<Ctx>({ space: 'life', setSpace: () => {} });

export function SpaceProvider({ children }: { children: ReactNode }) {
  const [space, setSpaceState] = useState<Space>(initial);
  const setSpace = (s: Space) => { localStorage.setItem(KEY, s); setSpaceState(s); };
  return <SpaceCtx.Provider value={{ space, setSpace }}>{children}</SpaceCtx.Provider>;
}

export function useSpace(): Ctx { return useContext(SpaceCtx); }
