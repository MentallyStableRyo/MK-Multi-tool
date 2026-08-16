/**
 * The contract between the app shell and whatever occupies a tab.
 *
 * The shell used to speak only `PlatformView`, because every tab was a video
 * source. Remove Background is not one — it downloads nothing, has no URL, and
 * ends in a PNG rather than a slice — so the parts the shell actually depends on
 * are lifted here as {@link Tool}. The shell now knows how to show a panel,
 * lend it the action bar, and mark it busy; it knows nothing else about any of
 * them.
 *
 * {@link TOOL_TABS} is the tab strip in order. Adding a tool is an entry here
 * plus a class implementing {@link Tool}.
 */

import { PLATFORMS, type PlatformId } from "./platforms";
import type { JobEvent } from "./types";

export type ToolId = PlatformId | "removebg";

/** What the shared action bar should show for the active tool. */
export interface Actions {
  canChoose: boolean;
  canRender: boolean;
  busy: boolean;
  path: string;
  /** Left button's label. Defaults to the slice wording. */
  chooseLabel?: string;
  /** Right button's label when idle. */
  renderLabel?: string;
  /** Right button's label while `busy`. */
  busyLabel?: string;
}

/** The services a tool needs from the app shell around it. */
export interface Chrome {
  /** Push action-bar state. The shell ignores it unless this tool is active. */
  setActions(tool: Tool, actions: Actions): void;
  /**
   * Offer a pasted link to the tab that owns it. Returns true if another tab
   * took it, in which case the caller should leave the paste alone.
   */
  route(url: string, from: Tool): boolean;
  /** Whether the sidecars were found at boot. */
  ready(): boolean;
  /** Default save location, resolved once during boot. */
  readonly defaults: { dir: string; separator: string };
}

/** One tab's workspace. Instances share nothing and keep state while hidden. */
export interface Tool {
  readonly id: ToolId;

  /** Called when this tab becomes visible. */
  activate(): void;
  /** Called when another tab takes over. */
  deactivate(): void;
  /** Re-evaluate the controls after something outside the tool changed. */
  refresh(): void;

  /** Whether long-running work is in flight; drives the tab's busy dot. */
  readonly busy: boolean;

  /** The action bar's left button. */
  chooseDestination(): void | Promise<void>;
  /** The action bar's right button. */
  startRender(): void | Promise<void>;

  /** Backend job progress. Only tools that start backend jobs implement it. */
  handleProgress?(event: JobEvent): void;
  /** Take a link routed here from another tab. Only URL-driven tools have one. */
  adopt?(url: string): void;
}

export interface TabSpec {
  readonly id: ToolId;
  readonly label: string;
  /** Tab glyph: inner markup for a 16×16 `viewBox`, drawn in `currentColor`. */
  readonly icon: string;
  /**
   * Draw a divider before this tab. Marks the seam between the video-source
   * tabs and the tools that merely happen to live in the same strip, so the
   * grouping is legible without a second row of chrome.
   */
  readonly startsGroup?: boolean;
}

/**
 * A mountain inside a frame with a corner lifted away — the picture, and the
 * background being taken off it.
 */
const REMOVEBG_ICON =
  '<path d="M2.2 10.2l3.1-3.1 2.5 2.5 2-2 2.2 2.2" fill="none" stroke="currentColor" ' +
  'stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/>' +
  '<circle cx="10.3" cy="5.2" r="1.05" fill="currentColor"/>' +
  '<path d="M13.8 8.6v3.1a2.1 2.1 0 0 1-2.1 2.1H4.3a2.1 2.1 0 0 1-2.1-2.1V4.3a2.1 2.1 0 0 1 2.1-2.1h3.1" ' +
  'fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>' +
  '<path d="M10.4 2.2h3.4v3.4" fill="none" stroke="currentColor" stroke-width="1.4" ' +
  'stroke-linecap="round" stroke-linejoin="round" stroke-dasharray="1.6 1.5"/>';

export const TOOL_TABS: readonly TabSpec[] = [
  ...PLATFORMS.map((platform) => ({
    id: platform.id,
    label: platform.label,
    icon: platform.icon,
  })),
  {
    id: "removebg",
    label: "Remove BG",
    icon: REMOVEBG_ICON,
    startsGroup: true,
  },
];
