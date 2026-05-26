import { lazy, Suspense } from 'react';
import { Routes, Route, Navigate } from 'react-router-dom';
import { getToken } from '@/lib/api';
import { Login } from '@/pages/Login';
import { Marketing } from '@/pages/Marketing';
import { Notes } from '@/pages/Notes';
import { Plans } from '@/pages/Plans';
import { Search } from '@/pages/Search';
import { Profile } from '@/pages/Profile';
import { NoteView } from '@/pages/NoteView';
import { AppShell } from '@/components/app-shell';
import { SpaceProvider } from '@/components/space-context';

// Heavy (MDXEditor) — load its chunk only on the editor route.
const NoteEditor = lazy(() =>
  import('@/pages/NoteEditor').then((m) => ({ default: m.NoteEditor })),
);
const editorEl = (
  <Suspense fallback={null}>
    <NoteEditor />
  </Suspense>
);

function RequireAuth({ children }: { children: React.ReactNode }) {
  return getToken() ? <>{children}</> : <Navigate to="/login" replace />;
}

export default function App() {
  return (
    <Routes>
      <Route path="/" element={<Marketing />} />
      <Route path="/login" element={<Login />} />
      <Route
        path="/app"
        element={
          <RequireAuth>
            <SpaceProvider>
              <AppShell />
            </SpaceProvider>
          </RequireAuth>
        }
      >
        <Route index element={<Notes />} />
        <Route path="notes/new" element={editorEl} />
        <Route path="notes/:id" element={<NoteView />} />
        <Route path="notes/:id/edit" element={editorEl} />
        <Route path="plans" element={<Plans />} />
        <Route path="search" element={<Search />} />
        <Route path="profile" element={<Profile />} />
      </Route>
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
