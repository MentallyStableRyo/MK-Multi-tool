/**
 * Application shell: the tab strip, the shared chrome, and boot.
 *
 * The shell owns nothing about slicing, and nothing about image matting. It
 * builds one {@link Tool} per entry in {@link TOOL_TABS}, shows exactly one of
 * them at a time, and lends the active one the action bar at the bottom of the
 * window. Every tool keeps its own state whether or not it is on screen, so a
 * render started in one tab carries on reporting while the user works in
 * another.
 */

import { api, onJobProgress, toErrorPayload } from "./bridge";
import { DEFAULT_PLATFORM, PLATFORMS, platformFor } from "./platforms";
import { RemoveBackgroundView } from "./removebg";
import { TOOL_TABS, type Actions, type Chrome, type Tool, type ToolId } from "./tool";
import type { ErrorPayload, JobEvent } from "./types";
import { PlatformView } from "./view";

// ----------------------------------------------------------------- markup

const app = document.querySelector<HTMLDivElement>("#app");
if (!app) throw new Error("#app mount point is missing");

app.innerHTML = `
  <header class="topbar">
    <div class="brand">MK <span>Multi-tool</span></div>
    <div class="spacer"></div>
    <button class="pill" id="toolPill" title="Show detected tool versions">
      <i class="dot busy" id="toolDot"></i><span id="toolText">Checking tools…</span>
    </button>
  </header>

  <nav class="tabs" id="tabs" role="tablist" aria-label="Tools">
    ${TOOL_TABS.map(
      (tab, index) => `
      ${tab.startsGroup ? '<i class="tab-divider" aria-hidden="true"></i>' : ""}
      <button type="button" class="tab" role="tab" data-tool="${tab.id}"
              id="tab-${tab.id}" aria-controls="panel-${tab.id}"
              aria-selected="${index === 0}" tabindex="${index === 0 ? 0 : -1}">
        <svg class="tab-icon" viewBox="0 0 16 16" aria-hidden="true">${tab.icon}</svg>
        <span>${tab.label}</span>
        <i class="tab-busy" title="Working" hidden></i>
      </button>`,
    ).join("")}
  </nav>

  <div class="alert error shell-alert" id="shellAlert" hidden>
    <strong id="shellAlertMessage"></strong>
    <span class="alert-hint" id="shellAlertHint"></span>
  </div>

  <main id="panels"></main>

  <footer class="actionbar">
    <button class="ghost" id="chooseBtn" disabled>Save to…</button>
    <span class="path-display" id="pathDisplay">No destination selected</span>
    <button class="primary" id="renderBtn" disabled>Render slice</button>
  </footer>
`;

const $ = <T extends HTMLElement>(id: string): T => {
  const node = document.getElementById(id);
  if (!node) throw new Error(`missing element #${id}`);
  return node as T;
};

const ui = {
  toolDot: $("toolDot"),
  toolText: $("toolText"),
  toolPill: $<HTMLButtonElement>("toolPill"),

  tabs: $("tabs"),
  panels: $("panels"),

  alert: $("shellAlert"),
  alertMessage: $("shellAlertMessage"),
  alertHint: $("shellAlertHint"),

  chooseBtn: $<HTMLButtonElement>("chooseBtn"),
  pathDisplay: $("pathDisplay"),
  renderBtn: $<HTMLButtonElement>("renderBtn"),
};

// ------------------------------------------------------------------ state

/** Resolved once at boot and shared by every view as its starting location. */
const defaults = { dir: "", separator: "/" };

let toolchainReady = false;
let active: Tool | null = null;

const views = new Map<ToolId, Tool>();
const tabs = new Map<ToolId, HTMLButtonElement>();

/** Shell-level failures only — a view's own errors stay inside its panel. */
function showShellError(payload: ErrorPayload | null): void {
  if (!payload) {
    ui.alert.hidden = true;
    return;
  }
  ui.alert.hidden = false;
  ui.alertMessage.textContent = payload.message;
  ui.alertHint.textContent = payload.hint ?? "";
  ui.alertHint.hidden = !payload.hint;
}

// ------------------------------------------------------------------ views

const chrome: Chrome = {
  setActions(tool: Tool, actions: Actions): void {
    syncTabBadges();
    // A background tab may finish a render at any moment; its footer state is
    // simply not the one on screen.
    if (tool !== active) return;

    ui.chooseBtn.disabled = !actions.canChoose;
    ui.chooseBtn.textContent = actions.chooseLabel ?? "Save to…";

    ui.renderBtn.disabled = !actions.canRender;
    // Each tool names its own verb — the strip mixes rendering a slice with
    // exporting a PNG, and a footer that said "Render slice" on both would be
    // lying on one of them.
    ui.renderBtn.textContent = actions.busy
      ? actions.busyLabel ?? "Rendering…"
      : actions.renderLabel ?? "Render slice";

    ui.pathDisplay.textContent = actions.path;
  },

  route(url: string, from: Tool): boolean {
    const platform = platformFor(url);
    if (!platform || platform.id === from.id) return false;

    const target = views.get(platform.id);
    if (!target?.adopt) return false;

    // The link belongs to another service, so send the user there rather than
    // rejecting a paste they clearly meant.
    select(platform.id);
    target.adopt(url);
    return true;
  },

  ready: () => toolchainReady,
  defaults,
};

for (const spec of TOOL_TABS) {
  const panel = document.createElement("section");
  panel.id = `panel-${spec.id}`;
  panel.setAttribute("aria-labelledby", `tab-${spec.id}`);
  panel.hidden = true;
  ui.panels.append(panel);

  const platform = PLATFORMS.find((candidate) => candidate.id === spec.id);
  const view: Tool = platform
    ? new PlatformView(panel, platform, chrome)
    : new RemoveBackgroundView(panel, chrome);

  views.set(spec.id, view);
  tabs.set(spec.id, $<HTMLButtonElement>(`tab-${spec.id}`));
}

function select(id: ToolId): void {
  const next = views.get(id);
  if (!next || next === active) return;

  active?.deactivate();
  active = next;

  for (const [toolId, tab] of tabs) {
    const selected = toolId === id;
    tab.classList.toggle("active", selected);
    tab.setAttribute("aria-selected", String(selected));
    tab.tabIndex = selected ? 0 : -1;
  }

  next.activate();
}

/** Mark tabs whose tool has work in flight. */
function syncTabBadges(): void {
  for (const [id, view] of views) {
    const badge = tabs.get(id)?.querySelector<HTMLElement>(".tab-busy");
    if (badge) badge.hidden = !view.busy;
  }
}

// ----------------------------------------------------------------- wiring

ui.tabs.addEventListener("click", (event) => {
  const tab = (event.target as HTMLElement).closest<HTMLButtonElement>(".tab");
  const id = tab?.dataset.tool as ToolId | undefined;
  if (id) select(id);
});

// Arrow keys move along the strip, as a tablist is expected to.
ui.tabs.addEventListener("keydown", (event) => {
  const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
  if (step === 0 || !active) return;

  event.preventDefault();
  const order = [...views.keys()];
  const index = order.indexOf(active.id);
  const next = order[(index + step + order.length) % order.length];
  if (next) {
    select(next);
    tabs.get(next)?.focus();
  }
});

ui.chooseBtn.addEventListener("click", () => void active?.chooseDestination());
ui.renderBtn.addEventListener("click", () => void active?.startRender());

// ------------------------------------------------------------------- boot

function syncToolchain(ready: boolean, label: string, tooltip: string): void {
  toolchainReady = ready;
  ui.toolDot.className = `dot ${ready ? "ok" : "err"}`;
  ui.toolText.textContent = label;
  ui.toolPill.title = tooltip;
  // Every source tab's Analyse button is gated on the sidecars being present.
  // Remove Background needs none of them, and ignores this.
  for (const view of views.values()) view.refresh();
}

async function boot(): Promise<void> {
  // One subscription for the whole app; each view keeps only the events for
  // the job it started.
  void onJobProgress((event: JobEvent) => {
    for (const view of views.values()) view.handleProgress?.(event);
  });

  select(DEFAULT_PLATFORM);

  try {
    const dir = await api.defaultOutputDir();
    defaults.dir = dir;
    defaults.separator = dir.includes("\\") ? "\\" : "/";
  } catch {
    defaults.dir = "";
  }

  try {
    const status = await api.checkToolchain();
    const missing = status.tools.filter((t) => !t.found).map((t) => t.name);
    const tooltip = status.tools
      .map((t) => `${t.name}: ${t.version ?? t.path ?? "not found"}`)
      .join("\n");

    syncToolchain(
      status.ready,
      status.ready ? "Tools ready" : `Missing: ${missing.join(", ")}`,
      tooltip,
    );

    if (!status.ready) {
      showShellError({
        kind: "missing_binary",
        message: `Required tools are unavailable: ${missing.join(", ")}.`,
        hint: "Run `npm run binaries` to vendor them into src-tauri/binaries, or install them so they are on your PATH.",
      });
    }
  } catch (raw) {
    syncToolchain(false, "Tool check failed", "");
    showShellError(toErrorPayload(raw));
  }
}

void boot();
