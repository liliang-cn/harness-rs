import { lazy, Suspense } from 'react';
import { Routes, Route, Navigate } from 'react-router-dom';
import { getToken } from '@/lib/api';
import { Login } from '@/pages/Login';
import { Marketing } from '@/pages/Marketing';
import { Dashboard } from '@/pages/Dashboard';
import { Ledger } from '@/pages/Ledger';
import { Portfolio } from '@/pages/Portfolio';
import { Profile } from '@/pages/Profile';
import { Projects } from '@/pages/Projects';
import { ProjectView } from '@/pages/ProjectView';
import { NoteView } from '@/pages/NoteView';
import { AppShell } from '@/components/app-shell';

// NoteEditor is lazy — will eventually use MDXEditor (heavy bundle)
const NoteEditor = lazy(() =>
  import('@/pages/NoteEditor').then((m) => ({ default: m.NoteEditor })),
);

// `/` is always the Marketing page — one URL, one content. Authenticated
// app lives under `/app/*` (the shell + nested routes). A logged-in user
// visiting `/` sees Marketing too (they can click `Open app` to jump
// in); login redirects to `/app`. This keeps the SEO landing stable
// regardless of session state and avoids the "same URL different content"
// surprise.
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
            <AppShell />
          </RequireAuth>
        }
      >
        <Route index element={<Dashboard />} />

        {/* Money tab — transactions + portfolio subroute */}
        <Route path="money" element={<Ledger />} />
        <Route path="money/portfolio" element={<Portfolio />} />

        {/* Projects tab */}
        <Route path="projects" element={<Projects />} />
        <Route path="projects/:id" element={<ProjectView />} />

        {/* Notes (accessed via project view) */}
        <Route path="notes/:id" element={<NoteView />} />
        <Route
          path="notes/:id/edit"
          element={
            <Suspense fallback={null}>
              <NoteEditor />
            </Suspense>
          }
        />
        <Route
          path="notes/new"
          element={
            <Suspense fallback={null}>
              <NoteEditor />
            </Suspense>
          }
        />

        <Route path="profile" element={<Profile />} />

        {/* Backwards-compat aliases so old /app/ledger + /app/portfolio links don't 404 */}
        <Route path="ledger" element={<Navigate to="/app/money" replace />} />
        <Route path="portfolio" element={<Navigate to="/app/money/portfolio" replace />} />
      </Route>
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
}
