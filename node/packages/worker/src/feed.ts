// Minimal RSS/Atom parser — no dependency, good enough for the fun rewrite.
// Pulls guid/title/link/description out of <item> (RSS) or <entry> (Atom).

export interface FeedItem {
  guid: string;
  title: string;
  url?: string;
  body?: string;
}

const tag = (block: string, name: string): string | undefined => {
  const m = block.match(new RegExp(`<${name}[^>]*>([\\s\\S]*?)</${name}>`, "i"));
  if (!m) return undefined;
  return decode(m[1].replace(/<!\[CDATA\[([\s\S]*?)\]\]>/g, "$1").trim());
};

const attr = (block: string, name: string, a: string): string | undefined => {
  const m = block.match(new RegExp(`<${name}[^>]*\\b${a}="([^"]+)"`, "i"));
  return m?.[1];
};

function decode(s: string): string {
  return s
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/&amp;/g, "&");
}

export function parseFeed(xml: string): FeedItem[] {
  const blocks = xml.match(/<(item|entry)[\s\S]*?<\/(item|entry)>/gi) ?? [];
  const items: FeedItem[] = [];
  for (const b of blocks) {
    const title = tag(b, "title") ?? "(untitled)";
    const link = tag(b, "link") ?? attr(b, "link", "href");
    const guid = tag(b, "guid") ?? tag(b, "id") ?? link ?? title;
    const body = tag(b, "description") ?? tag(b, "summary") ?? tag(b, "content");
    items.push({ guid, title, url: link, body });
  }
  return items;
}
