// Minimal HTML scraper — no heavy dependency.
// Extracts candidate items from a page: anchor links + headings (h1..h3).
// Each item has a stable guid = the resolved absolute URL (for links) or a
// title-based key (for headings with no URL), matching the dedup pattern in
// store.ingestScrape.

export interface ScrapeItem {
  guid: string;
  title: string;
  url: string;
}

function stripTags(s: string): string {
  return s.replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").trim();
}

function resolveUrl(href: string, base: string): string | null {
  if (!href || href.startsWith("#") || href.startsWith("javascript:")) return null;
  try {
    return new URL(href, base).href;
  } catch {
    return null;
  }
}

function decodeEntities(s: string): string {
  return s
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/&amp;/g, "&");
}

export function parsePage(html: string, baseUrl: string): ScrapeItem[] {
  const items: ScrapeItem[] = [];
  const seen = new Set<string>();

  // Anchors: <a href="...">text</a>
  for (const m of html.matchAll(/<a\s[^>]*href="([^"]*)"[^>]*>([\s\S]*?)<\/a>/gi)) {
    const href = m[1].trim();
    const rawText = m[2];
    const title = decodeEntities(stripTags(rawText));
    if (title.length < 3) continue;
    const url = resolveUrl(href, baseUrl);
    if (!url) continue;
    const guid = url;
    if (seen.has(guid)) continue;
    seen.add(guid);
    items.push({ guid, title, url });
  }

  // Headings h1..h3
  for (const m of html.matchAll(/<(h[1-3])\b[^>]*>([\s\S]*?)<\/h[1-3]>/gi)) {
    const rawText = m[2];
    const title = decodeEntities(stripTags(rawText));
    if (title.length < 3) continue;
    // Use title as a stable key; prefix with baseUrl to scope it to this source.
    const guid = `heading:${baseUrl}:${title}`;
    if (seen.has(guid)) continue;
    seen.add(guid);
    items.push({ guid, title, url: baseUrl });
  }

  return items;
}
