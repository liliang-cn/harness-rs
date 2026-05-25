import { Link } from 'react-router-dom';
import { getToken } from '@/lib/api';
import {
  ArrowRight,
  Search,
  MessageSquare,
  Layers,
  ShieldCheck,
  BookOpen,
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
// renders identically on a cold crawl — important for SEO + GEO. CN-first
// per the positioning; bilingual copy where helpful.

const FEATURES = [
  {
    icon: Search,
    title: '语义搜索 · Semantic search',
    body: '不用记关键词。用自然语言描述你的想法，ai-note 会在所有笔记中找到最接近的内容 — 哪怕措辞完全不同。',
  },
  {
    icon: Layers,
    title: '工作·生活分区 · Work · Life spaces',
    body: '工作笔记和生活笔记各自独立，互不干扰。切换 Space 即刻切换上下文；AI 助手只在当前分区内搜索和记录。',
  },
  {
    icon: MessageSquare,
    title: '流式对话捕捉 · Streaming chat capture',
    body: '和 AI 说一句话就能记下一条笔记。对话历史持久保存，随时重打开继续。AI 读懂你的笔记，答案来自你自己写的内容。',
  },
  {
    icon: ShieldCheck,
    title: '自托管，单二进制 · Self-hosted, single binary',
    body: 'ai-note 是一个静态 Rust 二进制加嵌入式 UI。跑在 $5 VPS 上。所有数据存在一个你掌控的 SQLite 文件里，不依赖任何第三方云。',
  },
];

const HOW = [
  {
    n: '1',
    title: '注册 / Sign up',
    body: '邮箱 + 密码即可，可选邀请码解锁付费功能。',
  },
  {
    n: '2',
    title: '选择分区 · Choose a space',
    body: '工作 or 生活 — 一键切换，笔记和对话各自隔离。',
  },
  {
    n: '3',
    title: '说一句，记下来',
    body: '直接打字给 AI，它帮你记；或自己写笔记，再问 AI。',
  },
];

const COMPARISON = [
  ['捕捉方式', '手动打开 App 写', 'AI 对话即可捕捉'],
  ['搜索方式', '关键词过滤', '自然语言语义搜索'],
  ['对话记忆', '无', '会话持久，随时续接'],
  ['空间隔离', '单一收件箱', '工作 · 生活双分区'],
  ['数据所有权', '厂商云端', '自托管 · 一个 SQLite 文件'],
];

const FAQ = [
  {
    q: 'ai-note 是什么？',
    a: 'ai-note 是一个 AI 驱动的笔记工具。你可以通过自然语言对话快速捕捉想法，也可以直接写笔记；语义搜索让你用自己的语言找到任何内容。',
  },
  {
    q: '工作和生活分区有什么用？',
    a: '分区将笔记、搜索、对话历史完全隔离。切换到工作空间时，AI 只看工作笔记；生活笔记不会出现在工作上下文里，反之亦然。',
  },
  {
    q: '数据存在哪里？',
    a: '存在你运行 ai-note 的服务器上的一个 SQLite 文件里。自托管则数据完全不离开你的基础设施。托管版数据存储于亚太地区（东京）的专用服务器。',
  },
  {
    q: '哪个 AI 模型驱动助手？',
    a: '默认是 DeepSeek v4-flash，嵌入模型使用 Gemini 3.5-flash。付费用户可以在个人资料页切换至 DeepSeek v4-pro 或 Gemini 3.5-pro。',
  },
  {
    q: '费用是多少？',
    a: '试用层免费，有笔记条数和对话次数限制。付费层去掉限制，并解锁模型升级选项。',
  },
  {
    q: 'UI 支持哪些语言？',
    a: '界面支持中文和英文。AI 助手理解两种语言，并用用户输入的语言回复。',
  },
  {
    q: '可以导出数据吗？',
    a: '可以。在个人资料页一键导出所有笔记为 .zip 压缩包。',
  },
];

export function Marketing() {
  const authed = !!getToken();
  return (
    <div className="bg-background min-h-svh">
      {/* Top bar */}
      <header className="border-border sticky top-0 z-10 flex h-14 items-center gap-3 border-b bg-background/80 px-4 backdrop-blur sm:px-8">
        <Link to="/" className="text-lg font-semibold tracking-tight">
          ai-note
        </Link>
        <span className="text-muted-foreground hidden text-xs sm:inline">
          AI 语义笔记
        </span>
        <div className="flex-1" />
        {authed ? (
          <Button asChild size="sm">
            <Link to="/app">
              打开应用 <ArrowRight className="size-4" />
            </Link>
          </Button>
        ) : (
          <>
            <Button asChild variant="ghost" size="sm">
              <Link to="/login">登录</Link>
            </Button>
            <Button asChild size="sm">
              <Link to="/login">
                免费开始 <ArrowRight className="size-4" />
              </Link>
            </Button>
          </>
        )}
      </header>

      {/* Hero */}
      <section className="mx-auto max-w-4xl px-4 pt-16 pb-12 text-center sm:pt-24 sm:pb-20">
        <Badge variant="secondary" className="mb-6">
          为个人和小团队打造
        </Badge>
        <h1 className="text-4xl font-bold tracking-tight sm:text-5xl md:text-6xl">
          你的 AI 笔记 —<br />
          说一句就记下，问一句就找到。
        </h1>
        <p className="text-muted-foreground mx-auto mt-6 max-w-2xl text-lg sm:text-xl">
          工作和生活双分区隔离；语义搜索让你用自己的语言找到任何内容；AI
          对话即可捕捉想法，会话历史永久保存。
        </p>
        <div className="mt-8 flex flex-wrap justify-center gap-3">
          <Button asChild size="lg">
            <Link to="/login">
              免费开始 <ArrowRight />
            </Link>
          </Button>
          <Button asChild size="lg" variant="outline">
            <a href="#how">了解工作原理</a>
          </Button>
        </div>
        <p className="text-muted-foreground mt-4 text-xs">
          免费试用 · 无需信用卡 · 中文 / English
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
          <h2 className="text-3xl font-bold tracking-tight sm:text-4xl">工作原理</h2>
          <p className="text-muted-foreground mt-3">三步，从零到 AI 笔记。</p>
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
              ai-note 与普通笔记应用的区别
            </h2>
            <p className="text-muted-foreground mt-3">
              印象笔记、Notion、Bear 是存储工具。ai-note 是会思考的助手。
            </p>
          </div>
          <Card>
            <CardContent className="p-0">
              <div className="grid grid-cols-3 gap-px overflow-hidden rounded-lg bg-border text-sm">
                <div className="bg-card text-muted-foreground p-4 font-medium">能力</div>
                <div className="bg-card text-muted-foreground p-4 font-medium">
                  普通笔记应用
                </div>
                <div className="bg-card text-foreground p-4 font-semibold">ai-note</div>
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
            常见问题
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
          <BookOpen className="text-muted-foreground mx-auto mb-4 size-10" />
          <h2 className="text-3xl font-bold tracking-tight sm:text-4xl">
            别让想法溜走。
          </h2>
          <p className="text-muted-foreground mx-auto mt-3 max-w-xl">
            30 秒注册，免费试用，无需信用卡。
          </p>
          <Button asChild size="lg" className="mt-6">
            <Link to="/login">
              免费开始 <ArrowRight />
            </Link>
          </Button>
        </div>
      </section>

      {/* Footer */}
      <footer className="text-muted-foreground mx-auto flex max-w-6xl flex-col items-center justify-between gap-3 px-4 py-8 text-xs sm:flex-row">
        <span>© {new Date().getFullYear()} ai-note · superleo.app</span>
        <div className="flex gap-4">
          <a href="/llms.txt" target="_blank" rel="noopener">
            llms.txt
          </a>
          <a href="/sitemap.xml" target="_blank" rel="noopener">
            sitemap
          </a>
          <Link to="/login">登录</Link>
        </div>
      </footer>
    </div>
  );
}
