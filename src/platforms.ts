/**
 * The sources the app can slice from, and everything that differs between them.
 *
 * One entry here becomes one tab, one isolated `PlatformView`, and one
 * `Platform` on the Rust side (`src-tauri/src/platform.rs`) — the `id` strings
 * must match that enum's serde names. Anything platform-specific belongs in
 * this table rather than in a conditional inside the view, so adding a fifth
 * site is a data change.
 */

export type PlatformId = "youtube" | "instagram" | "tiktok" | "twitter";

export interface Platform {
  readonly id: PlatformId;
  readonly label: string;
  /** Tab glyph: inner markup for a 16×16 `viewBox`, drawn in `currentColor`. */
  readonly icon: string;
  readonly placeholder: string;
  /**
   * Recognises a link belonging to this platform. Used only for routing a
   * paste to the right tab — the backend does the authoritative validation.
   */
  readonly matches: RegExp;
  /**
   * Whether an embeddable, seekable player exists for this site.
   *
   * Only YouTube has one: its embed accepts player commands over `postMessage`,
   * which is what makes the live preview possible. Instagram, TikTok, and X
   * either refuse to frame at all or expose no seek control, so a "preview"
   * there could only be a still image pretending to be a player. The section is
   * therefore built for YouTube alone, and simply does not exist elsewhere.
   */
  readonly preview: boolean;
  /**
   * Whether the source box also accepts a keyword search.
   *
   * yt-dlp exposes a first-class `ytsearch:` scheme, so YouTube gets this for
   * free. The other three have no equivalent that returns structured results
   * without scraping a search page, so their box stays link-only.
   */
  readonly search: boolean;
  /** Seconds pre-selected after analysis; `"full"` takes the whole clip. */
  readonly defaultSpan: number | "full";
  /** What a signed-in session buys here, shown under Advanced. */
  readonly cookieNote: string;
}

export const PLATFORMS: readonly Platform[] = [
  {
    id: "youtube",
    label: "YouTube",
    icon:
      '<rect x="1.5" y="3.75" width="13" height="8.5" rx="2.75" fill="none" stroke="currentColor" stroke-width="1.4"/>' +
      '<path d="M6.9 6.2v3.6L10.1 8z" fill="currentColor"/>',
    placeholder: "Paste a YouTube link, or type to search",
    matches: /(?:^|\/\/|\.)(?:youtube\.com|youtu\.be)/i,
    preview: true,
    search: true,
    defaultSpan: 30,
    cookieNote:
      "Cookies let the app open private, members-only, or age-restricted videos using a session " +
      "you are already signed into.",
  },
  {
    id: "instagram",
    label: "Instagram",
    icon:
      '<rect x="2.2" y="2.2" width="11.6" height="11.6" rx="3.4" fill="none" stroke="currentColor" stroke-width="1.4"/>' +
      '<circle cx="8" cy="8" r="2.85" fill="none" stroke="currentColor" stroke-width="1.4"/>' +
      '<circle cx="11.5" cy="4.5" r="0.95" fill="currentColor"/>',
    placeholder: "Paste an Instagram link — post, reel, or IGTV",
    matches: /(?:^|\/\/|\.)instagram\.com/i,
    preview: false,
    search: false,
    defaultSpan: "full",
    cookieNote:
      "Instagram serves almost nothing to a signed-out client, so a browser session is usually " +
      "required rather than optional.",
  },
  {
    id: "tiktok",
    label: "TikTok",
    icon:
      '<path d="M9.3 2.2v7.2a2.35 2.35 0 1 1-2-2.32" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/>' +
      '<path d="M9.3 2.2c.35 1.75 1.6 2.85 3.4 3.05" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>',
    placeholder: "Paste a TikTok link — a video URL or a vm./vt. short link",
    matches: /(?:^|\/\/|\.)(?:tiktok\.com)/i,
    preview: false,
    search: false,
    defaultSpan: "full",
    cookieNote:
      "Cookies help with posts that are region-locked or restricted to followers, and reduce how " +
      "often TikTok rate-limits the extractor.",
  },
  {
    id: "twitter",
    label: "X",
    icon:
      '<path d="M3.4 3.4l9.2 9.2M12.6 3.4l-9.2 9.2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>',
    placeholder: "Paste an X link — a post URL containing a video",
    matches: /(?:^|\/\/|\.)(?:twitter\.com|x\.com)/i,
    preview: false,
    search: false,
    defaultSpan: "full",
    cookieNote:
      "X hides media on protected accounts and some age-gated posts from signed-out clients; " +
      "browser cookies unlock those.",
  },
];

export const DEFAULT_PLATFORM: PlatformId = "youtube";

/** The platform a pasted link belongs to, or `null` if none claims it. */
export function platformFor(url: string): Platform | null {
  return PLATFORMS.find((p) => p.matches.test(url.trim())) ?? null;
}
