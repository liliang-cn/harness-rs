import { Link } from 'react-router-dom';
import { getToken } from '@/lib/api';
import {
  ArrowRight,
  LineChart,
  MessageSquare,
  ShieldCheck,
  Target,
  Check,
} from 'lucide-react';
import { Button } from '@/components/ui/button';
import { Badge } from '@/components/ui/badge';
import { Card, CardContent } from '@/components/ui/card';
import {
  Accordion,
  AccordionContent,
  AccordionItem,
  AccordionTrigger,
} from '@/components/ui/accordion';

// Marketing copy intentionally lives in plain JSX (not i18n keys) so it
// renders identically on a cold crawl — important for SEO + GEO. EN-first
// per the 海外 positioning; a /zh route can be added later if traffic
// from the CN search engines justifies it.

const FEATURES = [
  {
    icon: LineChart,
    title: 'Money at a glance',
    body: 'Cash, brokerage, credit, loans — aggregated into your net worth, refreshed daily. Track income, expenses, budgets, and subscriptions. Switch currencies and every figure re-converts at the latest ECB mid rate.',
  },
  {
    icon: Target,
    title: 'Projects with progress',
    body: 'Create projects, break them into milestones, log progress reviews with a cadence you set. The AI can create and update projects from a single chat message.',
  },
  {
    icon: MessageSquare,
    title: 'AI that knows your context',
    body: "Ask \"how much did I spend on infrastructure last quarter?\" or \"what's the status of my SaaS launch project?\" The assistant has tools to query your actual data — not generic heuristics.",
  },
  {
    icon: ShieldCheck,
    title: 'Self-hosted, single binary',
    body: 'Dashboard is one static Rust binary plus an embedded UI. Run it on a $5 VPS. All data lives in one SQLite file you control. Nothing leaves your infrastructure.',
  },
];

const HOW = [
  {
    n: '1',
    title: 'Sign up',
    body: 'Email + password. Optional invite code unlocks the paid tier.',
  },
  {
    n: '2',
    title: 'Add what matters',
    body: 'Add accounts and transactions, or create your first project. Each account carries its own currency.',
  },
  {
    n: '3',
    title: 'Ask the AI',
    body: 'Chat in English or Chinese. The assistant reads your actual numbers and project data.',
  },
];

const COMPARISON = [
  ['Net-worth view', 'Cash only / single currency', 'All accounts · multi-currency'],
  ['Project tracking', 'None', 'Milestones · reviews · cadence'],
  ['Insight delivery', 'You read charts', 'AI surfaces patterns'],
  ['Query interface', 'Filter UI', 'Natural language (EN / ZH)'],
  ['Data ownership', 'Vendor cloud', 'Self-hostable · one SQLite file'],
];

const FAQ = [
  {
    q: 'Who is Dashboard for?',
    a: 'Independent developers, one-person companies, and small teams who want a single place to track money, projects, and notes — without juggling three separate tools.',
  },
  {
    q: 'Does Dashboard connect to my bank?',
    a: 'Not yet. The current release uses manual entry or CSV import. Direct integrations (Plaid, TrueLayer, SaltEdge) are on the paid-tier roadmap.',
  },
  {
    q: 'Where is my data stored?',
    a: 'In a single SQLite file on the server you run Dashboard on. Self-host and it never leaves your infrastructure. The hosted version stores data on dedicated servers in Asia-Pacific (Tokyo).',
  },
  {
    q: 'Which AI model powers the assistant?',
    a: 'Default is DeepSeek v4-flash with Gemini 3.5-flash as the embedding model. Paid users can switch to DeepSeek v4-pro or Gemini 3.5-pro from their profile.',
  },
  {
    q: 'How much does it cost?',
    a: 'The trial tier is free with limits on accounts and transactions. The paid tier removes limits and unlocks model upgrades, scheduled monthly reports, and bank-account integrations once they ship.',
  },
  {
    q: 'What languages does the UI support?',
    a: 'English and Chinese (Simplified). The AI assistant understands both and replies in whichever language the user wrote in.',
  },
  {
    q: 'Can I export my data?',
    a: 'Yes. Transactions, trades, and subscriptions all export to CSV from the user menu.',
  },
];

export function Marketing() {
  const authed = !!getToken();
  return (
    <div className="bg-background min-h-svh">
      {/* Top bar */}
      <header className="border-border sticky top-0 z-10 flex h-14 items-center gap-3 border-b bg-background/80 px-4 backdrop-blur sm:px-8">
        <Link to="/" className="text-lg font-semibold tracking-tight">
          Dashboard
        </Link>
        <span className="text-muted-foreground hidden text-xs sm:inline">
          AI cockpit for solo operators
        </span>
        <div className="flex-1" />
        {authed ? (
          <Button asChild size="sm">
            <Link to="/app">
              Open app <ArrowRight className="size-4" />
            </Link>
          </Button>
        ) : (
          <>
            <Button asChild variant="ghost" size="sm">
              <Link to="/login">Sign in</Link>
            </Button>
            <Button asChild size="sm">
              <Link to="/login">
                Start free <ArrowRight className="size-4" />
              </Link>
            </Button>
          </>
        )}
      </header>

      {/* Hero */}
      <section className="mx-auto max-w-4xl px-4 pt-16 pb-12 text-center sm:pt-24 sm:pb-20">
        <Badge variant="secondary" className="mb-6">
          Built for independent developers &amp; one-person companies
        </Badge>
        <h1 className="text-4xl font-bold tracking-tight sm:text-5xl md:text-6xl">
          Your AI cockpit for money, projects, and notes.
        </h1>
        <p className="text-muted-foreground mx-auto mt-6 max-w-2xl text-lg sm:text-xl">
          Stop juggling three tools. Dashboard gives you net worth, income &amp;
          cost, project tracking, and an AI co-pilot — all in one self-hosted
          binary.
        </p>
        <div className="mt-8 flex flex-wrap justify-center gap-3">
          <Button asChild size="lg">
            <Link to="/login">
              Start free <ArrowRight />
            </Link>
          </Button>
          <Button asChild size="lg" variant="outline">
            <a href="#how">See how it works</a>
          </Button>
        </div>
        <p className="text-muted-foreground mt-4 text-xs">
          Free trial · No credit card · Bilingual (English / 中文)
        </p>
      </section>

      {/* Features */}
      <section className="mx-auto max-w-6xl px-4 py-12 sm:py-20">
        <div className="grid gap-4 sm:grid-cols-2">
          {FEATURES.map((f) => (
            <Card key={f.title}>
              <CardContent className="p-6">
                <div className="bg-secondary text-secondary-foreground mb-4 flex size-10 items-center justify-center rounded-lg">
                  <f.icon className="size-5" />
                </div>
                <h3 className="mb-2 text-lg font-semibold">{f.title}</h3>
                <p className="text-muted-foreground text-sm leading-relaxed">{f.body}</p>
              </CardContent>
            </Card>
          ))}
        </div>
      </section>

      {/* How it works */}
      <section id="how" className="mx-auto max-w-4xl px-4 py-12 sm:py-20">
        <div className="mb-10 text-center">
          <h2 className="text-3xl font-bold tracking-tight sm:text-4xl">How it works</h2>
          <p className="text-muted-foreground mt-3">From zero to your cockpit in three steps.</p>
        </div>
        <ol className="grid gap-6 sm:grid-cols-3">
          {HOW.map((step) => (
            <li key={step.n} className="border-border bg-card rounded-lg border p-6">
              <div className="bg-foreground text-background mb-3 flex size-8 items-center justify-center rounded-full text-sm font-semibold">
                {step.n}
              </div>
              <h3 className="mb-2 font-semibold">{step.title}</h3>
              <p className="text-muted-foreground text-sm">{step.body}</p>
            </li>
          ))}
        </ol>
      </section>

      {/* Comparison */}
      <section className="bg-muted/30 border-border border-y py-12 sm:py-20">
        <div className="mx-auto max-w-5xl px-4">
          <div className="mb-10 text-center">
            <h2 className="text-3xl font-bold tracking-tight sm:text-4xl">
              How Dashboard differs from passive trackers
            </h2>
            <p className="text-muted-foreground mt-3">
              Notion, 随手记, YNAB are journals. Dashboard is a cockpit.
            </p>
          </div>
          <Card>
            <CardContent className="p-0">
              <div className="grid grid-cols-3 gap-px overflow-hidden rounded-lg bg-border text-sm">
                <div className="bg-card text-muted-foreground p-4 font-medium">Capability</div>
                <div className="bg-card text-muted-foreground p-4 font-medium">
                  Passive trackers
                </div>
                <div className="bg-card text-foreground p-4 font-semibold">Dashboard</div>
                {COMPARISON.map(([cap, theirs, ours]) => (
                  <div className="contents" key={cap}>
                    <div className="bg-card p-4">{cap}</div>
                    <div className="bg-card text-muted-foreground p-4">{theirs}</div>
                    <div className="bg-card p-4">
                      <span className="inline-flex items-start gap-1.5">
                        <Check className="text-emerald-600 mt-0.5 size-4 shrink-0 dark:text-emerald-400" />
                        <span>{ours}</span>
                      </span>
                    </div>
                  </div>
                ))}
              </div>
            </CardContent>
          </Card>
        </div>
      </section>

      {/* FAQ */}
      <section className="mx-auto max-w-3xl px-4 py-12 sm:py-20">
        <div className="mb-10 text-center">
          <h2 className="text-3xl font-bold tracking-tight sm:text-4xl">
            Frequently asked questions
          </h2>
        </div>
        <Accordion type="single" collapsible className="w-full">
          {FAQ.map((item, i) => (
            <AccordionItem value={`q-${i}`} key={item.q}>
              <AccordionTrigger className="text-left">{item.q}</AccordionTrigger>
              <AccordionContent>{item.a}</AccordionContent>
            </AccordionItem>
          ))}
        </Accordion>
      </section>

      {/* CTA */}
      <section className="border-border bg-card border-y py-12 sm:py-20">
        <div className="mx-auto max-w-3xl px-4 text-center">
          <Target className="text-muted-foreground mx-auto mb-4 size-10" />
          <h2 className="text-3xl font-bold tracking-tight sm:text-4xl">
            One cockpit for everything that matters.
          </h2>
          <p className="text-muted-foreground mx-auto mt-3 max-w-xl">
            Sign up in 30 seconds. Free trial. No credit card.
          </p>
          <Button asChild size="lg" className="mt-6">
            <Link to="/login">
              Start free <ArrowRight />
            </Link>
          </Button>
        </div>
      </section>

      {/* Footer */}
      <footer className="text-muted-foreground mx-auto flex max-w-6xl flex-col items-center justify-between gap-3 px-4 py-8 text-xs sm:flex-row">
        <span>© {new Date().getFullYear()} Dashboard · superleo.app</span>
        <div className="flex gap-4">
          <a href="/llms.txt" target="_blank" rel="noopener">
            llms.txt
          </a>
          <a href="/sitemap.xml" target="_blank" rel="noopener">
            sitemap
          </a>
          <Link to="/login">Sign in</Link>
        </div>
      </footer>
    </div>
  );
}
