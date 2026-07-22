import { getCollection } from "astro:content";
import { OGImageRoute } from "astro-og-canvas";
import { ogCardConfig } from "./_og-card-config";
import { localeFromEntryId } from "../../lib/i18n";

const entries = await getCollection(
  "docs",
  (entry) => !entry.data.draft && localeFromEntryId(entry.id) === "",
);

const pages = Object.fromEntries(
  entries.map((entry) => [
    entry.id,
    {
      title: entry.data.title,
      description: entry.data.description ?? "",
    },
  ]),
);

export const { getStaticPaths, GET } = await OGImageRoute({
  pages,
  getImageOptions: (_path, page) => ({
    title: page.title,
    description: page.description,
    ...ogCardConfig,
  }),
});
