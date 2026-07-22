import type { SidebarConfigItem, SidebarItem } from "@cloudflare/nimbus-docs/types";

export const LOCALES = {
  "": { code: "en", label: "English" },
  ko: { code: "ko", label: "한국어" },
  ja: { code: "ja", label: "日本語" },
  "zh-cn": { code: "zh-CN", label: "简体中文" },
} as const;

export type Locale = keyof typeof LOCALES;

type LocalizedLabel = {
  label: string;
  translations?: Partial<Record<Exclude<Locale, "">, string>>;
};

type NavigationLink = LocalizedLabel & {
  slug: string;
};

type NavigationGroup = LocalizedLabel & {
  items: NavigationLink[];
};

export const NAVIGATION: NavigationGroup[] = [
  {
    label: "Getting Started",
    translations: { ko: "시작하기", ja: "はじめに", "zh-cn": "开始使用" },
    items: [
      { label: "Why shunt", translations: { ko: "shunt이란?", ja: "shunt とは", "zh-cn": "为什么选择 shunt" }, slug: "getting-started/why-shunt" },
      { label: "Comparison", translations: { ko: "비교", ja: "比較", "zh-cn": "对比" }, slug: "getting-started/comparison" },
      { label: "Installation", translations: { ko: "설치", ja: "インストール", "zh-cn": "安装" }, slug: "getting-started/installation" },
      { label: "Quickstart", translations: { ko: "빠른 시작", ja: "クイックスタート", "zh-cn": "快速开始" }, slug: "getting-started/quickstart" },
    ],
  },
  {
    label: "Providers",
    translations: { ko: "프로바이더", ja: "プロバイダー", "zh-cn": "提供方" },
    items: [
      { label: "Overview", translations: { ko: "개요", ja: "概要", "zh-cn": "概览" }, slug: "guides/providers" },
      { label: "Anthropic", slug: "providers/anthropic" },
      { label: "OpenAI", slug: "providers/openai" },
      { label: "ChatGPT / Codex", translations: { ko: "ChatGPT / Codex", ja: "ChatGPT / Codex", "zh-cn": "ChatGPT / Codex" }, slug: "guides/codex" },
      { label: "xAI / Grok", translations: { ko: "xAI / Grok", ja: "xAI / Grok", "zh-cn": "xAI / Grok" }, slug: "guides/xai" },
      { label: "Cursor", slug: "providers/cursor" },
      { label: "Kimi (Moonshot)", slug: "providers/kimi" },
      { label: "DeepSeek", slug: "providers/deepseek" },
      { label: "Z.ai (GLM)", slug: "providers/zai" },
      { label: "MiniMax", slug: "providers/minimax" },
      { label: "Mimo (Xiaomi)", slug: "providers/mimo" },
      { label: "OpenRouter", slug: "providers/openrouter" },
      { label: "Vercel AI Gateway", slug: "providers/vercel-ai-gateway" },
    ],
  },
  {
    label: "Guides",
    translations: { ko: "가이드", ja: "ガイド", "zh-cn": "指南" },
    items: [
      { label: "Configuration", translations: { ko: "설정", ja: "設定", "zh-cn": "配置" }, slug: "guides/configuration" },
      { label: "Anthropic Multi-Account", translations: { ko: "Anthropic 멀티 계정", ja: "Anthropic マルチアカウント", "zh-cn": "Anthropic 多账户" }, slug: "guides/anthropic-multi-account" },
      { label: "Codex Multi-Account", slug: "guides/codex-multi-account" },
      { label: "Inbound Codex Endpoint", slug: "guides/inbound-codex-endpoint" },
      { label: "Admin & Remote Provisioning", translations: { ko: "관리자 & 원격 프로비저닝", ja: "管理とリモートプロビジョニング", "zh-cn": "管理与远程预配" }, slug: "guides/admin-remote-provisioning" },
      { label: "Gateway Login", slug: "guides/gateway-login" },
      { label: "Connect Claude Code", translations: { ko: "Claude Code 연결", ja: "Claude Code の接続", "zh-cn": "连接 Claude Code" }, slug: "guides/connect-claude-code" },
      { label: "Connect Claude Desktop", translations: { ko: "Claude Desktop 연결", ja: "Claude Desktop の接続", "zh-cn": "连接 Claude Desktop" }, slug: "guides/connect-claude-desktop" },
      { label: "Connect the Codex CLI", translations: { ko: "Codex CLI 연결", ja: "Codex CLI の接続", "zh-cn": "连接 Codex CLI" }, slug: "guides/connect-codex-cli" },
      { label: "Model Discovery", translations: { ko: "모델 디스커버리", ja: "モデルディスカバリー", "zh-cn": "模型发现" }, slug: "guides/model-discovery" },
      { label: "Effort & Context", translations: { ko: "Effort와 컨텍스트", ja: "Effort とコンテキスト", "zh-cn": "推理强度与上下文" }, slug: "guides/effort-and-context" },
      { label: "Sharing a Gateway", translations: { ko: "게이트웨이 공유", ja: "ゲートウェイの共有", "zh-cn": "共享网关" }, slug: "guides/shared-gateway" },
      { label: "OpenTelemetry", translations: { ko: "OpenTelemetry", ja: "OpenTelemetry", "zh-cn": "OpenTelemetry" }, slug: "guides/opentelemetry" },
    ],
  },
  {
    label: "Reference",
    translations: { ko: "레퍼런스", ja: "リファレンス", "zh-cn": "参考" },
    items: [
      { label: "CLI", translations: { ko: "CLI", ja: "CLI", "zh-cn": "CLI" }, slug: "reference/cli" },
      { label: "Configuration Reference", translations: { ko: "설정 레퍼런스", ja: "設定リファレンス", "zh-cn": "配置参考" }, slug: "reference/configuration" },
      { label: "HTTP Endpoints", translations: { ko: "HTTP 엔드포인트", ja: "HTTP エンドポイント", "zh-cn": "HTTP 端点" }, slug: "reference/endpoints" },
      { label: "Troubleshooting", translations: { ko: "문제 해결", ja: "トラブルシューティング", "zh-cn": "故障排查" }, slug: "reference/troubleshooting" },
    ],
  },
];

const normalizePath = (path: string): string => {
  const normalized = `/${path}`.replace(/\/+/g, "/").replace(/\/$/, "");
  return normalized || "/";
};

const translatedLabel = (item: LocalizedLabel, locale: Locale): string =>
  locale === "" ? item.label : item.translations?.[locale] ?? item.label;

export const ENGLISH_SIDEBAR_ITEMS: SidebarConfigItem[] = NAVIGATION.map((group) => ({
  label: group.label,
  items: group.items.map((item) => ({ label: item.label, link: item.slug })),
}));

export function localeFromSlug(slug: string): Locale {
  const firstSegment = slug.replace(/^\//, "").split("/", 1)[0];
  return firstSegment === "ko" || firstSegment === "ja" || firstSegment === "zh-cn"
    ? firstSegment
    : "";
}

export const localeFromEntryId = localeFromSlug;

export function stripLocalePrefix(slug: string): string {
  const locale = localeFromSlug(slug);
  if (!locale) return slug.replace(/^\//, "").replace(/\/$/, "");
  return slug
    .replace(/^\//, "")
    .replace(new RegExp(`^${locale}(?:/|$)`), "")
    .replace(/\/$/, "");
}

export function localizedPath(locale: Locale, slug: string): string {
  const bareSlug = stripLocalePrefix(slug);
  return normalizePath([locale, bareSlug].filter(Boolean).join("/"));
}

export function buildLocaleSidebar(locale: Locale, currentSlug: string): SidebarItem[] {
  const currentPath = normalizePath(currentSlug);
  let order = 0;

  return NAVIGATION.map((group) => ({
    type: "group" as const,
    label: translatedLabel(group, locale),
    order: order++,
    children: group.items.map((item) => {
      const href = localizedPath(locale, item.slug);
      return {
        type: "link" as const,
        label: translatedLabel(item, locale),
        href,
        isCurrent: currentPath === href,
        order: order++,
      };
    }),
  }));
}
