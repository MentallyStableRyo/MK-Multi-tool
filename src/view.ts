/**
 * One platform's workspace — source, metadata, preview, slice, and progress.
 *
 * Every tab owns an instance of this, and instances share nothing: separate
 * DOM, separate state, separate in-flight job. Switching tabs is therefore just
 * showing a different panel; a half-configured Instagram slice is exactly where
 * you left it when you come back from YouTube.
 *
 * The DOM is built once and then patched — no virtual DOM, no re-render — and
 * every mutation funnels through one of the `sync*` methods, so the UI can
 * never drift from `this.state`. Elements are addressed by `data-el` rather
 * than `id`, because four live copies of this markup exist at once.
 */

import { save } from "@tauri-apps/plugin-dialog";

import { api, toErrorPayload } from "./bridge";
import type { Platform } from "./platforms";
import { Preview } from "./preview";
import type { Chrome, Tool, ToolId } from "./tool";
import { Timeline, type Handle } from "./timeline";
import {
  estimateSize,
  formatBytes,
  formatClock,
  formatEta,
  formatPrecise,
  parseClock,
  validateRange,
} from "./time";
import {
  AUDIO_CONTAINERS,
  type Container,
  type ErrorPayload,
  type JobEvent,
  type Precision,
  type QualityTrack,
  type SearchResult,
  type VideoInfo,
} from "./types";

interface ViewState {
  info: VideoInfo | null;
  track: QualityTrack | null;
  start: number;
  end: number;
  container: Container;
  precision: Precision;
  outputDir: string;
  outputName: string;
  separator: string;
  cookieBrowser: string;
  cookieFile: string;
  transcodeAv1: boolean;
  jobId: string | null;
  analyzing: boolean;
  searching: boolean;
  /**
   * Job id for the in-flight analyse or search.
   *
   * Separate from `jobId`, which belongs to the render: a metadata fetch and a
   * slice can never be cancelled by the same button, and conflating them would
   * let one cancel the other.
   */
  sourceJobId: string | null;
  results: SearchResult[];
}

/** Seconds of context shown before the cut point by the "End scene" button. */
const BOUNDARY_WINDOW = 4;

const PREVIEW_PLACEHOLDER =
  "Watch the exact range before rendering — straight from YouTube, nothing downloaded.";

const STAGE_LABELS: Record<JobEvent["stage"], string> = {
  resolving: "Resolving streams",
  planning: "Analysing keyframes",
  rendering: "Rendering slice",
  finalizing: "Finalising",
  done: "Complete",
  failed: "Failed",
  cancelled: "Cancelled",
};

type PreviewKind = "slice" | "start" | "end";

/**
 * Whether the source box holds a link rather than a search phrase.
 *
 * Deliberately loose: the backend does the authoritative validation, and this
 * only has to choose which of the two requests to send. A scheme is not
 * required, so `youtu.be/xyz` still reads as a link — but a trailing path
 * segment is, which keeps a query like "node.js tutorial" out of the URL path.
 */
function looksLikeUrl(text: string): boolean {
  return /^[a-z][a-z0-9+.-]*:\/\//i.test(text) || /^[\w-]+(?:\.[\w-]+)+\//.test(text);
}

/**
 * Short view count for a dense result row — `1.2M` rather than `1,234,567`.
 *
 * The metadata card keeps the full `toLocaleString` form; it has one video to
 * describe and the room to do it.
 */
function formatCount(n: number): string {
  return new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 })
    .format(n);
}

// ---------------------------------------------------------------- markup

function markup(platform: Platform): string {
  // The preview is a YouTube-only step, and it sits above the slice controls:
  // you decide where to cut by watching, so the picture belongs above the
  // handles rather than below them.
  const step = platform.preview
    ? { video: 2, preview: 3, slice: 4 }
    : { video: 2, preview: 0, slice: 3 };

  const preview = platform.preview
    ? `
    <section class="card" data-el="previewCard" hidden>
      <h2 class="card-title">${step.preview} · Preview</h2>
      <div class="preview-bar">
        <button class="chip accent" data-el="previewSlice">▶ Play slice</button>
        <button class="chip" data-el="previewStart">Start scene</button>
        <button class="chip" data-el="previewEnd">End scene</button>
        <span class="grow"></span>
        <button class="chip ghost" data-el="previewClose" hidden>Close</button>
      </div>
      <div class="preview-stage" data-el="previewStage">
        <p class="preview-placeholder">${PREVIEW_PLACEHOLDER}</p>
      </div>
      <p class="hint" data-el="previewNote"></p>
    </section>`
    : "";

  return `
    <div class="alert error" data-el="alert" hidden>
      <strong data-el="alertMessage"></strong>
      <span class="alert-hint" data-el="alertHint"></span>
    </div>

    <section class="card">
      <h2 class="card-title">1 · Source</h2>
      <div class="row">
        <input class="grow" type="${platform.search ? "text" : "url"}" data-el="urlInput"
               spellcheck="false" autocomplete="off" placeholder="${platform.placeholder}" />
        <button class="primary" data-el="analyzeBtn">Analyse</button>
        <button class="danger" data-el="sourceCancelBtn" hidden>Cancel</button>
      </div>
      ${
        platform.search
          ? `
      <p class="hint" data-el="searchNote" hidden></p>
      <div class="results" data-el="searchResults" hidden role="list"></div>`
          : ""
      }
    </section>

    <section class="card" data-el="metaCard" hidden>
      <h2 class="card-title">${step.video} · Video</h2>
      <div class="meta">
        <img class="thumb" data-el="thumb" alt="" />
        <div class="meta-body">
          <div class="meta-title" data-el="videoTitle"></div>
          <div class="meta-sub" data-el="videoChannel"></div>
          <div class="meta-stats">
            <span data-el="videoDuration"></span>
            <span data-el="videoViews"></span>
          </div>
        </div>
      </div>
      <div class="row" style="margin-top:16px">
        <label class="field grow">
          Quality
          <select data-el="qualitySelect"></select>
        </label>
        <label class="field" style="min-width:150px">
          Container
          <select data-el="containerSelect">
            <option value="mp4">MP4 · H.264/AAC</option>
            <option value="mkv">MKV · any codec</option>
            <option value="webm">WebM · VP9/Opus</option>
            <option value="m4a">M4A · audio only</option>
            <option value="mp3">MP3 · audio only</option>
          </select>
        </label>
      </div>
      <div class="hint" data-el="compatHint" style="margin-top:10px"></div>
    </section>
${preview}
    <section class="card" data-el="rangeCard" hidden>
      <h2 class="card-title">${step.slice} · Slice</h2>
      <div data-el="timeline"></div>
      <div class="range-grid">
        <div>
          <label class="field">
            Start
            <input type="text" class="mono" data-el="startInput" value="0:00" spellcheck="false" />
          </label>
          <div class="nudges" data-target="start">
            <button data-delta="-10">−10s</button>
            <button data-delta="-1">−1s</button>
            <button data-delta="1">+1s</button>
            <button data-delta="10">+10s</button>
          </div>
          <div class="field-error" data-el="startError"></div>
        </div>
        <div>
          <label class="field">
            End
            <input type="text" class="mono" data-el="endInput" value="0:10" spellcheck="false" />
          </label>
          <div class="nudges" data-target="end">
            <button data-delta="-10">−10s</button>
            <button data-delta="-1">−1s</button>
            <button data-delta="1">+1s</button>
            <button data-delta="10">+10s</button>
          </div>
          <div class="field-error" data-el="endError"></div>
        </div>
        <div class="readout">
          <span>Selection</span>
          <div class="value" data-el="sliceDuration">0.00</div>
          <span class="hint" data-el="sliceEstimate"></span>
        </div>
      </div>
    </section>

    <section class="card" data-el="advancedCard" hidden>
      <details class="advanced">
        <summary>Advanced</summary>
        <div class="content">
          <label class="field">
            Cut mode
            <select data-el="precisionSelect">
              <option value="auto">Auto — copy when aligned</option>
              <option value="exact">Exact — always frame-accurate</option>
              <option value="fast">Fast — never re-encode</option>
            </select>
          </label>
          <label class="field">
            Browser cookies
            <select data-el="cookieBrowser">
              <option value="">None</option>
              <option value="chrome">Chrome</option>
              <option value="edge">Edge</option>
              <option value="firefox">Firefox</option>
              <option value="brave">Brave</option>
              <option value="chromium">Chromium</option>
              <option value="opera">Opera</option>
              <option value="vivaldi">Vivaldi</option>
              <option value="safari">Safari</option>
            </select>
          </label>
          <label class="field">
            cookies.txt (overrides browser)
            <input type="text" data-el="cookieFile" spellcheck="false" placeholder="Optional file path" />
          </label>
          <label class="field check" data-el="transcodeAv1Field">
            Compatibility
            <span class="check-row">
              <input type="checkbox" data-el="transcodeAv1" />
              <span>Convert AV1 to H.264</span>
            </span>
          </label>
        </div>
        <p class="hint" data-el="av1Hint" style="margin-bottom:0"></p>
        <p class="hint" style="margin-bottom:0">
          ${platform.cookieNote} Nothing is uploaded — cookies are read locally and passed straight
          to yt-dlp.
        </p>
      </details>
    </section>

    <section class="card" data-el="progressCard" hidden>
      <div class="progress-head">
        <span class="progress-stage" data-el="progressStage">Working…</span>
        <span class="badge" data-el="progressBadge" hidden></span>
        <span class="progress-pct" data-el="progressPct">0%</span>
      </div>
      <div class="bar" data-el="progressBar"><i></i></div>
      <div class="progress-meta">
        <span data-el="progressSpeed">—</span>
        <span data-el="progressEta">—</span>
        <span data-el="progressBytes">—</span>
      </div>
      <div class="progress-note" data-el="progressNote"></div>
      <div class="row" style="margin-top:12px">
        <button class="danger" data-el="cancelBtn">Cancel</button>
        <button class="ghost" data-el="revealBtn" hidden>Show in folder</button>
      </div>
    </section>
  `;
}

// ------------------------------------------------------------------ view

export class PlatformView implements Tool {
  readonly platform: Platform;
  readonly id: ToolId;

  private readonly root: HTMLElement;
  private readonly chrome: Chrome;
  private readonly timeline: Timeline;
  /** Built only where an embeddable player exists — YouTube alone. */
  private readonly preview: Preview | null = null;

  private readonly state: ViewState;
  private lastOutputPath: string | null = null;
  private previewedRange: { start: number; end: number } | null = null;
  private previewKind: PreviewKind = "slice";
  private previewStatus: "connecting" | "live" | "degraded" = "connecting";

  private readonly el: Record<string, HTMLElement>;

  constructor(root: HTMLElement, platform: Platform, chrome: Chrome) {
    this.root = root;
    this.platform = platform;
    this.id = platform.id;
    this.chrome = chrome;

    this.state = {
      info: null,
      track: null,
      start: 0,
      end: 0,
      container: "mp4",
      precision: "auto",
      outputDir: "",
      outputName: "",
      separator: "/",
      cookieBrowser: "",
      cookieFile: "",
      transcodeAv1: false,
      jobId: null,
      analyzing: false,
      searching: false,
      sourceJobId: null,
      results: [],
    };

    root.classList.add("panel");
    root.setAttribute("role", "tabpanel");
    root.innerHTML = markup(platform);
    this.el = {};

    this.timeline = new Timeline(this.pick("timeline"), {
      onScrubStart: () => this.preview?.beginScrub(),
      onInput: (start, end, moving) => {
        this.state.start = start;
        this.state.end = end;
        this.input("startInput").value = formatClock(start);
        this.input("endInput").value = formatClock(end);
        this.trackCursor(moving);
        this.syncRange();
      },
      onCommit: () => {
        this.preview?.endScrub();
        void this.refreshFilename();
      },
    });

    if (platform.preview) {
      this.preview = new Preview(this.pick("previewStage"), PREVIEW_PLACEHOLDER, {
        onPlayhead: (seconds) => this.timeline.setPlayhead(seconds),
        onStatus: (status) => {
          this.previewStatus = status;
          this.syncPreviewNote();
        },
      });
    }

    this.wire();
    this.syncAv1Option();
    this.syncActions();
  }

  // --------------------------------------------------------- element refs

  private pick<T extends HTMLElement>(name: string): T {
    const cached = this.el[name];
    if (cached) return cached as T;

    const node = this.root.querySelector<T>(`[data-el="${name}"]`);
    if (!node) throw new Error(`${this.platform.id}: missing [data-el="${name}"]`);
    this.el[name] = node;
    return node;
  }

  private input(name: string): HTMLInputElement {
    return this.pick<HTMLInputElement>(name);
  }

  private select(name: string): HTMLSelectElement {
    return this.pick<HTMLSelectElement>(name);
  }

  // ------------------------------------------------------------ tab hooks

  /** Called when this tab becomes visible. */
  activate(): void {
    this.root.hidden = false;
    this.syncActions();
    // The whole flow starts with a paste, so put the caret where it belongs.
    this.input("urlInput").focus();
  }

  /** Re-evaluate the controls after something outside the view changed. */
  refresh(): void {
    this.syncActions();
  }

  /** Called when another tab takes over. */
  deactivate(): void {
    // A player left running behind a hidden panel would keep talking. One that
    // cannot be reached to be paused is dropped instead.
    if (this.preview && !this.preview.pause()) this.closePreview();
    this.root.hidden = true;
  }

  get busy(): boolean {
    return this.state.jobId !== null;
  }

  /** Take a link routed here from another tab, and analyse it immediately. */
  adopt(url: string): void {
    this.input("urlInput").value = url;
    void this.analyze();
  }

  // -------------------------------------------------------------- helpers

  private showError(payload: ErrorPayload | null): void {
    const alert = this.pick("alert");
    if (!payload) {
      alert.hidden = true;
      return;
    }
    alert.hidden = false;
    this.pick("alertMessage").textContent = payload.message;
    const hint = this.pick("alertHint");
    hint.textContent = payload.hint ?? "";
    hint.hidden = !payload.hint;
  }

  private joinPath(dir: string, name: string): string {
    if (!dir) return name;
    return `${dir.replace(/[\\/]+$/, "")}${this.state.separator}${name}`;
  }

  private currentOutputPath(): string {
    return this.joinPath(this.state.outputDir, this.state.outputName);
  }

  /** Adopt the shell's default destination the first time one is needed. */
  private ensureDestination(): void {
    if (this.state.outputDir) return;
    this.state.outputDir = this.chrome.defaults.dir;
    this.state.separator = this.chrome.defaults.separator;
  }

  private cookieOptions() {
    const browser = this.state.cookieBrowser.trim();
    const file = this.state.cookieFile.trim();
    if (!browser && !file) return null;
    return { browser: browser || null, file: file || null };
  }

  /** Mirrors `Container::accepts_video` in render.rs. */
  private containerAcceptsVideo(container: Container, family: string): boolean {
    switch (container) {
      case "mp4":
        return family === "h264" || family === "av1";
      case "mkv":
        return true;
      case "webm":
        return family === "vp9" || family === "vp8" || family === "av1";
      default:
        return false;
    }
  }

  /** Mirrors `Container::accepts_audio` in render.rs. */
  private containerAcceptsAudio(container: Container, family: string): boolean {
    switch (container) {
      case "mp4":
      case "m4a":
        return family === "aac";
      case "mkv":
        return true;
      case "webm":
        return family === "opus" || family === "vorbis";
      case "mp3":
        return family === "mp3";
    }
  }

  // -------------------------------------------------------------- preview

  /**
   * Bind the player's cursor to whichever edge of the slice just moved.
   *
   * This is the whole point of the live player: the seek is a message to an
   * already-running frame, so it costs nothing here and reloads nothing there.
   */
  private trackCursor(edge: Handle): void {
    const preview = this.preview;
    if (!preview?.open || preview.degraded) return;
    preview.setStop(this.state.end);
    preview.seek(edge === "start" ? this.state.start : this.state.end);
  }

  private focusPreview(kind: PreviewKind): void {
    const preview = this.preview;
    const { info, start, end } = this.state;
    if (!preview || !info) return;

    const at = kind === "end" ? Math.max(end - BOUNDARY_WINDOW, start) : start;

    if (!preview.open || preview.degraded) {
      // First open, or a player that cannot be steered: (re)load it in place.
      preview.load(info.id, at, end);
      this.previewedRange = { start, end };
    } else {
      preview.setStop(end);
      preview.seek(at);
      preview.play();
    }

    this.previewKind = kind;
    this.pick<HTMLButtonElement>("previewClose").hidden = false;
    this.syncPreviewNote();
    this.syncPreviewStaleness();
  }

  private closePreview(): void {
    if (!this.preview) return;
    this.preview.close();
    this.previewedRange = null;
    this.pick<HTMLButtonElement>("previewClose").hidden = true;
    this.pick("previewStage").classList.remove("stale");
    this.pick("previewNote").textContent = "";
    this.timeline.setPlayhead(null);
  }

  private syncPreviewNote(): void {
    const preview = this.preview;
    if (!preview?.open) return;

    const { start, end } = this.state;
    const range = `${formatClock(start)} → ${formatClock(end)}`;
    const scope =
      this.previewKind === "end"
        ? `Last ${BOUNDARY_WINDOW}s of ${range}`
        : `Reviewing ${range}`;

    const note = this.pick("previewNote");
    if (this.previewStatus === "live") {
      note.textContent =
        `${scope}. Drag either handle and the player follows it live — nothing reloads, and ` +
        `playback stops on the exact end you picked.`;
    } else if (this.previewStatus === "connecting") {
      note.textContent = `${scope}. Handing the player its controls…`;
    } else {
      note.textContent =
        `${scope}. This player would not accept live control, so it only seeks to whole ` +
        `seconds and needs replaying after you move the range. The rendered file still uses ` +
        `your exact times.`;
    }
  }

  /**
   * Flag the preview as out of date once the range moves under it.
   *
   * Only reachable on the fallback path: a live player is re-pointed as the
   * handles move, so it is never showing a range other than the current one.
   */
  private syncPreviewStaleness(): void {
    const preview = this.preview;
    if (!preview?.open || !this.previewedRange) return;

    const moved =
      preview.degraded &&
      (Math.abs(this.previewedRange.start - this.state.start) > 0.01 ||
        Math.abs(this.previewedRange.end - this.state.end) > 0.01);

    this.pick("previewStage").classList.toggle("stale", moved);
  }

  // --------------------------------------------------------------- syncing

  private syncMetadata(): void {
    const { info } = this.state;
    const visible = info !== null;

    this.pick("metaCard").hidden = !visible;
    this.pick("rangeCard").hidden = !visible;
    this.pick("advancedCard").hidden = !visible;
    if (this.platform.preview) this.pick("previewCard").hidden = !visible;
    if (!info) return;

    this.pick("videoTitle").textContent = info.title;
    this.pick("videoChannel").textContent = info.uploader;
    this.pick("videoDuration").textContent = `Length ${formatClock(info.durationSeconds, true)}`;
    this.pick("videoViews").textContent = info.viewCount
      ? `${info.viewCount.toLocaleString()} views`
      : "";

    const thumb = this.pick<HTMLImageElement>("thumb");
    if (info.thumbnail) {
      thumb.src = info.thumbnail;
      thumb.hidden = false;
    } else {
      thumb.removeAttribute("src");
      thumb.hidden = true;
    }

    this.timeline.setDuration(info.durationSeconds);
    this.timeline.setRange(this.state.start, this.state.end);
  }

  private syncCompatHint(): void {
    const hint = this.pick("compatHint");
    const { track, container } = this.state;
    if (!track) {
      hint.textContent = "";
      return;
    }

    if (AUDIO_CONTAINERS.has(container)) {
      hint.textContent = this.containerAcceptsAudio(container, track.audioCodec)
        ? "Audio is extracted without re-encoding — lossless and instant."
        : `Audio will be transcoded to ${container.toUpperCase()}.`;
      return;
    }

    if (track.audioOnly) {
      hint.textContent =
        "This is an audio-only track. Pick M4A or MP3, or choose a video quality.";
      return;
    }

    // "other" means the extractor never named the codec — X and TikTok often
    // don't. The backend measures it from the media before deciding how to cut,
    // so the honest answer here is that it depends on what it finds.
    if (track.videoCodec === "other" || track.audioCodec === "other") {
      hint.textContent =
        `This source doesn't fully report its codecs. They are read from the media itself when ` +
        `you render, and copied losslessly if they fit ${container.toUpperCase()}.`;
      return;
    }

    // An AV1 track the user has asked to convert is treated as not fitting,
    // which is exactly how the backend plans it.
    const forcedH264 =
      this.state.transcodeAv1 &&
      track.videoCodec === "av1" &&
      (container === "mp4" || container === "mkv");
    const videoFits = this.containerAcceptsVideo(container, track.videoCodec) && !forcedH264;
    const audioFits = this.containerAcceptsAudio(container, track.audioCodec);

    if (forcedH264) {
      hint.textContent =
        "AV1 will be re-encoded to H.264 for compatibility. Uncheck the option under Advanced to copy it losslessly instead.";
    } else if (!videoFits) {
      hint.textContent =
        `${track.videoCodec.toUpperCase()} cannot be stored in ${container.toUpperCase()}, ` +
        `so the video will be re-encoded. Choosing MKV would copy it losslessly instead.`;
    } else if (!audioFits) {
      hint.textContent =
        `Video copies losslessly; the ${track.audioCodec.toUpperCase()} audio will be ` +
        `re-encoded to fit ${container.toUpperCase()}.`;
    } else if (this.state.precision === "exact") {
      hint.textContent =
        "Exact mode always re-encodes the video so the cut lands on the requested frame.";
    } else {
      hint.textContent =
        "Both streams can be copied — the cut will be lossless if it lands on a keyframe.";
    }
  }

  private syncRange(): void {
    const duration = this.state.info?.durationSeconds ?? 0;
    const error = validateRange(this.state.start, this.state.end, duration);

    this.pick("startError").textContent = error?.field === "start" ? error.message : "";
    this.pick("endError").textContent = error?.field === "end" ? error.message : "";
    this.input("startInput").classList.toggle("invalid", error?.field === "start");
    this.input("endInput").classList.toggle("invalid", error?.field === "end");

    const length = Math.max(0, this.state.end - this.state.start);
    this.pick("sliceDuration").textContent = formatPrecise(length);

    const bytes = estimateSize(this.state.track?.bitrateKbps ?? 0, length);
    this.pick("sliceEstimate").textContent = bytes > 0 ? `≈ ${formatBytes(bytes)}` : "";

    this.syncPreviewStaleness();
    this.syncActions();
  }

  /**
   * The AV1 option only means something when the target container can hold
   * H.264 and the chosen track is actually AV1 — mirrors the backend rule in
   * `render.rs::plan_cut`.
   */
  private syncAv1Option(): void {
    const { container, track } = this.state;
    const holdsH264 = container === "mp4" || container === "mkv";
    const isAv1 = track?.videoCodec === "av1" && !track.audioOnly;
    const usable = holdsH264 && !AUDIO_CONTAINERS.has(container);

    this.input("transcodeAv1").disabled = !usable;
    this.pick("transcodeAv1Field").classList.toggle("disabled", !usable);

    const hint = this.pick("av1Hint");
    if (!usable) {
      hint.textContent =
        container === "webm"
          ? "WebM cannot store H.264, so this option does not apply to it."
          : "This option only applies to video containers.";
      return;
    }

    hint.textContent = isAv1
      ? this.state.transcodeAv1
        ? "The selected AV1 track will be re-encoded to H.264 — slower, and slightly lossy, but it plays on older TVs, phones, and editors."
        : "The selected track is AV1. It will be copied as-is, which is lossless and instant but unplayable on some older devices."
      : "Only applies when the selected quality uses AV1. The current track does not.";
  }

  private syncActions(): void {
    this.ensureDestination();

    const duration = this.state.info?.durationSeconds ?? 0;
    const rangeOk = validateRange(this.state.start, this.state.end, duration) === null;
    const busy = this.busy;

    const canRender =
      this.chrome.ready() &&
      this.state.info !== null &&
      this.state.track !== null &&
      rangeOk &&
      this.state.outputName !== "" &&
      !busy &&
      !this.state.info.isLive;

    const working = this.state.analyzing || this.state.searching;
    const analyzeBtn = this.pick<HTMLButtonElement>("analyzeBtn");
    analyzeBtn.disabled = working || busy || !this.chrome.ready();
    analyzeBtn.textContent = this.state.analyzing
      ? "Analysing…"
      : this.state.searching
        ? "Searching…"
        : this.searchMode()
          ? "Search"
          : "Analyse";

    // Only shown while something is in flight, so there is never a cancel
    // button sitting there with nothing to cancel.
    this.pick("sourceCancelBtn").hidden = !working;

    this.chrome.setActions(this, {
      canChoose: this.state.info !== null && !busy,
      canRender,
      busy,
      path: this.state.outputName ? this.currentOutputPath() : "No destination selected",
    });
  }

  // ------------------------------------------------------------- filenames

  private async refreshFilename(): Promise<void> {
    if (!this.state.info) return;
    try {
      this.state.outputName = await api.suggestFilename(
        this.state.info.title,
        this.state.start,
        this.state.end,
        this.state.container,
      );
    } catch {
      // A failed suggestion must not block the user; fall back to a plain name.
      this.state.outputName = `${this.platform.id}-slice.${this.state.container}`;
    }
    this.syncActions();
  }

  // -------------------------------------------------------------- analysis

  /** True when the box holds a search phrase rather than a link. */
  private searchMode(): boolean {
    return this.platform.search && !looksLikeUrl(this.input("urlInput").value.trim());
  }

  /** Route the box's contents: a link analyses, anything else searches. */
  private submitSource(): void {
    if (this.state.analyzing || this.state.searching) return;
    const text = this.input("urlInput").value.trim();
    if (!text) return;

    if (this.searchMode()) void this.search(text);
    else void this.analyze();
  }

  private async search(query: string): Promise<void> {
    const jobId = crypto.randomUUID();
    this.state.searching = true;
    this.state.sourceJobId = jobId;
    this.showError(null);
    this.setSearchNote("Searching…");
    this.syncActions();

    try {
      this.state.results = await api.searchYoutube(jobId, query, this.cookieOptions());
      this.renderResults(query);
    } catch (raw) {
      const error = toErrorPayload(raw);
      this.state.results = [];
      this.renderResults(query);
      // A cancel is the user's own doing, so it is a note rather than an error.
      if (error.kind === "cancelled") {
        this.setSearchNote("Search cancelled.");
      } else {
        this.setSearchNote("");
        this.showError(error);
      }
    } finally {
      this.state.searching = false;
      this.state.sourceJobId = null;
      this.syncActions();
    }
  }

  private async analyze(): Promise<void> {
    const url = this.input("urlInput").value.trim();
    if (!url || this.state.analyzing) return;

    const jobId = crypto.randomUUID();
    this.state.analyzing = true;
    this.state.sourceJobId = jobId;
    this.showError(null);
    // A player left open would be showing a completely different video.
    this.closePreview();
    this.syncActions();

    try {
      const info = await api.analyzeUrl(jobId, url, this.platform.id, this.cookieOptions());
      this.state.info = info;

      // Short-form posts are meant to be taken whole; long videos get a 30s
      // starting selection so the handles have somewhere sensible to sit.
      this.state.start = 0;
      const span = this.platform.defaultSpan;
      this.state.end =
        span === "full"
          ? info.durationSeconds || 30
          : Math.min(span, info.durationSeconds || span);

      this.populateQualities(info);
      this.syncMetadata();
      this.syncRange();
      this.input("startInput").value = formatClock(this.state.start);
      this.input("endInput").value = formatClock(this.state.end);
      await this.refreshFilename();

      if (info.isLive) {
        this.showError({
          kind: "live_stream",
          message: "This is a live or upcoming stream.",
          hint: "Live content has no fixed timeline to slice. Try again once the VOD is published.",
        });
      }
    } catch (raw) {
      const error = toErrorPayload(raw);
      this.state.info = null;
      this.state.track = null;
      this.syncMetadata();
      if (error.kind !== "cancelled") this.showError(error);
    } finally {
      this.state.analyzing = false;
      this.state.sourceJobId = null;
      this.syncActions();
    }
  }

  // --------------------------------------------------------------- search

  private setSearchNote(text: string): void {
    if (!this.platform.search) return;
    const note = this.pick("searchNote");
    note.textContent = text;
    note.hidden = text === "";
  }

  /**
   * Draw the result rows.
   *
   * Rows are buttons rather than links: picking one fills the box and runs the
   * normal analyse path, so a search is a way of reaching a URL rather than a
   * second, parallel route into the slice controls.
   */
  private renderResults(query: string): void {
    if (!this.platform.search) return;

    const list = this.pick("searchResults");
    list.replaceChildren();

    if (!this.state.results.length) {
      list.hidden = true;
      // Callers that end a search some other way — cancelled, failed — replace
      // this note afterwards, so it is safe to state the plain case here.
      this.setSearchNote(`No results for “${query}”.`);
      return;
    }

    for (const hit of this.state.results) {
      const row = document.createElement("button");
      row.type = "button";
      row.className = "result";
      row.setAttribute("role", "listitem");

      const thumb = document.createElement("img");
      thumb.className = "result-thumb";
      thumb.alt = "";
      thumb.loading = "lazy";
      if (hit.thumbnail) thumb.src = hit.thumbnail;

      const body = document.createElement("div");
      body.className = "result-body";

      const title = document.createElement("span");
      title.className = "result-title";
      title.textContent = hit.title;

      const meta = document.createElement("span");
      meta.className = "result-meta";
      meta.textContent = [
        hit.channel,
        hit.live ? "LIVE" : hit.duration ? formatClock(hit.duration) : null,
        hit.viewCount ? `${formatCount(hit.viewCount)} views` : null,
      ]
        .filter((part): part is string => Boolean(part))
        .join(" · ");

      body.append(title, meta);
      row.append(thumb, body);

      row.addEventListener("click", () => {
        this.input("urlInput").value = hit.url;
        this.clearResults();
        void this.analyze();
      });

      list.append(row);
    }

    list.hidden = false;
    this.setSearchNote(`${this.state.results.length} results — pick one to load it.`);
  }

  private clearResults(): void {
    if (!this.platform.search) return;
    this.state.results = [];
    this.pick("searchResults").replaceChildren();
    this.pick("searchResults").hidden = true;
    this.setSearchNote("");
  }

  private populateQualities(info: VideoInfo): void {
    const quality = this.select("qualitySelect");
    quality.replaceChildren();

    for (const track of info.tracks) {
      const option = document.createElement("option");
      option.value = track.id;
      option.textContent = `${track.label} — ${track.detail}${track.hdr ? " · HDR" : ""}`;
      quality.append(option);
    }

    // Prefer the best quality that stream-copies into MP4; otherwise the best
    // available. This makes the default selection the fast, lossless one.
    const preferred = info.tracks.find((t) => t.mp4Ready && !t.audioOnly) ?? info.tracks[0] ?? null;
    this.state.track = preferred;
    if (preferred) quality.value = preferred.id;

    this.syncAv1Option();
    this.syncCompatHint();
  }

  // ---------------------------------------------------------------- render

  /** Called by the action bar while this view is active. */
  async chooseDestination(): Promise<void> {
    const chosen = await save({
      defaultPath: this.currentOutputPath(),
      filters: [
        { name: this.state.container.toUpperCase(), extensions: [this.state.container] },
      ],
    });
    if (!chosen) return;

    const separatorIndex = Math.max(chosen.lastIndexOf("/"), chosen.lastIndexOf("\\"));
    if (separatorIndex > 0) {
      this.state.separator = chosen[separatorIndex] ?? this.state.separator;
      this.state.outputDir = chosen.slice(0, separatorIndex);
      this.state.outputName = chosen.slice(separatorIndex + 1);
    } else {
      this.state.outputName = chosen;
    }
    this.syncActions();
  }

  async startRender(): Promise<void> {
    const { info, track } = this.state;
    if (!info || !track || this.busy) return;

    this.showError(null);
    this.resetProgress();
    this.pick("progressCard").hidden = false;

    try {
      this.state.jobId = await api.startSlice({
        url: info.webpageUrl,
        platform: this.platform.id,
        trackId: track.id,
        start: this.state.start,
        end: this.state.end,
        durationSeconds: info.durationSeconds,
        container: this.state.container,
        precision: this.state.precision,
        transcodeAv1: this.state.transcodeAv1,
        outputPath: this.currentOutputPath(),
        bitrateKbps: track.bitrateKbps,
        title: info.title,
        cookies: this.cookieOptions(),
      });
    } catch (raw) {
      this.pick("progressCard").hidden = true;
      this.showError(toErrorPayload(raw));
    }
    this.syncActions();
  }

  private resetProgress(): void {
    this.lastOutputPath = null;
    this.pick("progressStage").textContent = "Starting…";
    this.pick("progressPct").textContent = "0%";

    const bar = this.pick("progressBar");
    bar.className = "bar indeterminate";
    (bar.firstElementChild as HTMLElement).style.width = "0%";

    this.pick("progressSpeed").textContent = "—";
    this.pick("progressEta").textContent = "—";
    this.pick("progressBytes").textContent = "—";
    this.pick("progressNote").textContent = "";
    this.pick("progressBadge").hidden = true;
    this.pick("cancelBtn").hidden = false;
    this.pick("revealBtn").hidden = true;
  }

  /**
   * Handle one backend progress event. The shell fans events out to every
   * view; only the one owning the job id reacts, so a render keeps reporting
   * while its tab sits in the background.
   */
  handleProgress(event: JobEvent): void {
    if (event.jobId !== this.state.jobId) return;

    this.pick("progressStage").textContent = STAGE_LABELS[event.stage];

    const bar = this.pick("progressBar");
    const determinate = event.percent !== null;
    bar.className = `bar${determinate ? "" : " indeterminate"}`;
    if (determinate) {
      const pct = event.percent ?? 0;
      (bar.firstElementChild as HTMLElement).style.width = `${pct}%`;
      this.pick("progressPct").textContent = `${pct.toFixed(pct >= 99.5 ? 0 : 1)}%`;
    }

    this.pick("progressSpeed").textContent = event.speed ?? "—";
    this.pick("progressEta").textContent =
      event.etaSeconds !== null ? `ETA ${formatEta(event.etaSeconds)}` : "—";
    this.pick("progressBytes").textContent =
      event.bytesWritten !== null ? formatBytes(event.bytesWritten) : "—";

    // Show the detail line for every in-flight stage. `resolving` carries the
    // retry notice, which is the one message the user most needs to see — a
    // silent 20-second retry looks identical to a stalled job.
    if (event.stage !== "done" && event.stage !== "failed" && event.stage !== "cancelled") {
      this.pick("progressNote").textContent = event.message;
    }

    if (event.lossless !== null) {
      const badge = this.pick("progressBadge");
      badge.hidden = false;
      badge.className = `badge ${event.lossless ? "lossless" : "encode"}`;
      badge.textContent = event.lossless ? "Stream copy" : "Re-encoding";
    }

    if (event.stage === "done") {
      bar.className = "bar done";
      (bar.firstElementChild as HTMLElement).style.width = "100%";
      this.pick("progressPct").textContent = "100%";
      this.pick("progressNote").textContent = event.outputPath ?? "";
      this.pick("cancelBtn").hidden = true;
      this.pick("revealBtn").hidden = false;
      this.lastOutputPath = event.outputPath;
      this.state.jobId = null;
      this.syncActions();
      return;
    }

    if (event.stage === "failed" || event.stage === "cancelled") {
      bar.className = "bar failed";
      this.pick("cancelBtn").hidden = true;
      this.pick("progressNote").textContent =
        event.stage === "cancelled" ? "Cancelled — nothing was written." : "";
      if (event.error) this.showError(event.error);
      this.state.jobId = null;
      this.syncActions();
    }
  }

  // ---------------------------------------------------------------- wiring

  private wire(): void {
    const urlInput = this.input("urlInput");

    this.pick("analyzeBtn").addEventListener("click", () => this.submitSource());

    urlInput.addEventListener("keydown", (e) => {
      if (e.key === "Enter") this.submitSource();
    });

    // The button reads "Search" or "Analyse" depending on what is typed, so it
    // has to re-evaluate as the box changes rather than only on submit.
    if (this.platform.search) {
      urlInput.addEventListener("input", () => this.syncActions());
    }

    this.pick("sourceCancelBtn").addEventListener("click", () => {
      if (this.state.sourceJobId) void api.cancelJob(this.state.sourceJobId);
    });

    // Pasting a link is an unambiguous intent to analyse it — and if it belongs
    // to another platform, to switch to that tab rather than fail here.
    urlInput.addEventListener("paste", (e) => {
      const text = e.clipboardData?.getData("text")?.trim();
      if (!text) return;
      if (this.chrome.route(text, this)) {
        // Another tab took it; leave this field exactly as the user had it.
        e.preventDefault();
        return;
      }
      if (!this.platform.matches.test(text)) return;
      setTimeout(() => void this.analyze(), 0);
    });

    this.select("qualitySelect").addEventListener("change", () => {
      const id = this.select("qualitySelect").value;
      const chosen = this.state.info?.tracks.find((t) => t.id === id) ?? null;
      this.state.track = chosen;

      // An audio-only track in a video container would produce a video-less
      // MP4; move the user to a sensible audio container instead.
      if (chosen?.audioOnly && !AUDIO_CONTAINERS.has(this.state.container)) {
        this.state.container = "m4a";
        this.select("containerSelect").value = "m4a";
        void this.refreshFilename();
      }

      this.syncAv1Option();
      this.syncCompatHint();
      this.syncRange();
    });

    this.select("containerSelect").addEventListener("change", () => {
      this.state.container = this.select("containerSelect").value as Container;
      this.syncAv1Option();
      this.syncCompatHint();
      void this.refreshFilename();
    });

    this.select("precisionSelect").addEventListener("change", () => {
      this.state.precision = this.select("precisionSelect").value as Precision;
      this.syncCompatHint();
    });

    this.select("cookieBrowser").addEventListener("change", () => {
      this.state.cookieBrowser = this.select("cookieBrowser").value;
    });

    this.input("cookieFile").addEventListener("input", () => {
      this.state.cookieFile = this.input("cookieFile").value;
    });

    this.input("transcodeAv1").addEventListener("change", () => {
      this.state.transcodeAv1 = this.input("transcodeAv1").checked;
      this.syncAv1Option();
      this.syncCompatHint();
    });

    if (this.platform.preview) {
      this.pick("previewSlice").addEventListener("click", () => this.focusPreview("slice"));
      this.pick("previewStart").addEventListener("click", () => this.focusPreview("start"));
      this.pick("previewEnd").addEventListener("click", () => this.focusPreview("end"));
      this.pick("previewClose").addEventListener("click", () => this.closePreview());
    }

    for (const field of ["start", "end"] as const) {
      const input = this.input(`${field}Input`);
      input.addEventListener("change", () => this.commitTimeInput(field));
      input.addEventListener("keydown", (e) => {
        if (e.key === "Enter") this.commitTimeInput(field);
      });
    }

    for (const group of this.root.querySelectorAll<HTMLElement>(".nudges")) {
      const field: Handle = group.dataset.target === "start" ? "start" : "end";
      group.addEventListener("click", (e) => this.nudge(e, field));
    }

    this.pick("cancelBtn").addEventListener("click", () => {
      if (this.state.jobId) void api.cancelJob(this.state.jobId);
    });

    this.pick("revealBtn").addEventListener("click", () => {
      if (this.lastOutputPath) void api.revealInFolder(this.lastOutputPath);
    });
  }

  /** Commit a typed timecode, snapping the field back to canonical form. */
  private commitTimeInput(field: Handle): void {
    const input = this.input(`${field}Input`);
    const parsed = parseClock(input.value);

    if (parsed === null) {
      input.classList.add("invalid");
      this.pick(`${field}Error`).textContent = "Use SS, MM:SS, or HH:MM:SS";
      this.syncActions();
      return;
    }

    const duration = this.state.info?.durationSeconds ?? 0;
    const clamped = duration > 0 ? Math.min(parsed, duration) : parsed;

    if (field === "start") this.state.start = clamped;
    else this.state.end = clamped;

    input.value = formatClock(clamped);
    this.timeline.setRange(this.state.start, this.state.end);
    this.trackCursor(field);
    this.syncRange();
    void this.refreshFilename();
  }

  private nudge(event: Event, field: Handle): void {
    const button = (event.target as HTMLElement).closest("button");
    const delta = Number(button?.dataset.delta ?? NaN);
    if (!Number.isFinite(delta)) return;

    const duration = this.state.info?.durationSeconds ?? 0;
    const next = Math.max(0, (field === "start" ? this.state.start : this.state.end) + delta);
    const clamped = duration > 0 ? Math.min(next, duration) : next;

    if (field === "start") this.state.start = Math.min(clamped, this.state.end - 0.1);
    else this.state.end = Math.max(clamped, this.state.start + 0.1);

    this.input("startInput").value = formatClock(this.state.start);
    this.input("endInput").value = formatClock(this.state.end);
    this.timeline.setRange(this.state.start, this.state.end);
    this.trackCursor(field);
    this.syncRange();
    void this.refreshFilename();
  }
}
