/**
 * The General Converter workspace — images, audio, video, documents, and
 * PDF ↔ image, all in one tab.
 *
 * Unlike the platform tabs, this one works on a *queue*: add one or more
 * files, pick one target format, and every queued file converts to it in
 * sequence into a chosen output folder. Filenames are auto-derived
 * (`<stem>.<targetExt>`) — there is no per-file destination picker, only a
 * folder, which is what makes converting twelve photos at once a single
 * click rather than twelve.
 *
 * The one exception to "one target format for the whole queue" is combining
 * several images into one PDF, which is a different *shape* of job (many
 * inputs, one output) rather than a different target format, and gets its own
 * toggle rather than living in the format dropdown.
 *
 * Every queued file must belong to the same family (`convert::Category`) —
 * mixing a JPEG and an MP3 in one batch has no single target format that
 * makes sense for both, so the queue blocks conversion and says so rather
 * than guessing.
 */

import { open } from "@tauri-apps/plugin-dialog";

import { api, toErrorPayload } from "./bridge";
import type { Chrome, Tool, ToolId } from "./tool";
import type {
  AudioFormat,
  Category,
  ConvertOptions,
  ConvertSummary,
  DocFormat,
  ErrorPayload,
  HardwareReport,
  ImageFormat,
  JobEvent,
  VideoFormat,
  VideoQuality,
} from "./types";

// ---------------------------------------------------------------- constants

/**
 * Extensions offered in the file picker, purely for the dialog's filter list.
 * Mirrors `convert::{IMAGE_EXT, AUDIO_EXT, VIDEO_EXT, DOC_EXT}` — the backend
 * (`convert_inspect`) is the actual authority on what category a file lands
 * in; a mismatch here only means a worse filter, never a wrong conversion.
 */
const IMAGE_EXT = ["png", "jpg", "jpeg", "webp", "bmp", "tiff", "tif", "gif", "ico", "svg"];
const AUDIO_EXT = ["mp3", "wav", "flac", "aac", "ogg", "opus", "m4a", "wma"];
const VIDEO_EXT = ["mp4", "mkv", "avi", "webm", "mov", "m4v", "flv", "wmv"];
const DOC_EXT = ["txt", "md", "markdown", "html", "htm", "docx", "odt", "rtf", "epub"];
const ALL_EXT = [...IMAGE_EXT, ...AUDIO_EXT, ...VIDEO_EXT, ...DOC_EXT, "pdf"];

const IMAGE_FORMATS: ReadonlyArray<[ImageFormat, string]> = [
  ["png", "PNG — lossless, transparency"],
  ["jpeg", "JPEG — lossy, smaller files"],
  ["webp", "WebP — lossless"],
  ["bmp", "BMP — uncompressed"],
  ["tiff", "TIFF"],
  ["gif", "GIF"],
  ["ico", "ICO — icon (capped at 256×256)"],
];

const AUDIO_FORMATS: ReadonlyArray<[AudioFormat, string]> = [
  ["mp3", "MP3"],
  ["wav", "WAV — uncompressed"],
  ["flac", "FLAC — lossless"],
  ["aac", "AAC"],
  ["ogg", "OGG (Vorbis)"],
  ["opus", "Opus"],
  ["m4a", "M4A (AAC)"],
];

const VIDEO_FORMATS: ReadonlyArray<[VideoFormat, string]> = [
  ["mp4", "MP4 — H.264, plays everywhere"],
  ["mkv", "MKV — H.264"],
  ["avi", "AVI — H.264"],
  ["webm", "WebM — VP9"],
  ["mov", "MOV — H.264"],
  ["gif", "GIF — silent, looping"],
];

const DOC_FORMATS: ReadonlyArray<[DocFormat, string]> = [
  ["txt", "Plain text"],
  ["markdown", "Markdown"],
  ["html", "HTML"],
  ["docx", "Word (.docx)"],
  ["odt", "OpenDocument (.odt)"],
  ["rtf", "Rich Text (.rtf)"],
  ["epub", "EPUB"],
];

const CATEGORY_LABEL: Record<Category, string> = {
  image: "Image",
  audio: "Audio",
  video: "Video",
  document: "Document",
  pdf: "PDF",
  unknown: "Unsupported",
};

type ItemStatus = "queued" | "converting" | "done" | "failed" | "skipped";

interface QueueItem {
  path: string;
  name: string;
  category: Category;
  pageCount: number | null;
  status: ItemStatus;
  message?: string;
}

interface State {
  items: QueueItem[];
  outputDir: string;
  separator: string;

  imageFormat: ImageFormat;
  imageQuality: number;

  audioFormat: AudioFormat;

  videoFormat: VideoFormat;
  videoQuality: VideoQuality;
  preferHardware: boolean;

  documentFormat: DocFormat;
  documentToPdf: boolean;

  pdfMode: "images" | "text";
  pdfImageFormat: ImageFormat;
  pdfDpi: number;
  pdfPages: string;

  combineImagesToPdf: boolean;
  combineOutputName: string;

  hardware: HardwareReport | null;
  hardwareLoaded: boolean;

  jobId: string | null;
  /** Index into `items` currently converting; `-1` for the combine job. */
  activeIndex: number | null;
  cancelledBatch: boolean;
}

const DEFAULT_JPEG_QUALITY = 90;
const DEFAULT_DPI = 150;

function baseName(path: string): string {
  const cut = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return cut >= 0 ? path.slice(cut + 1) : path;
}

function stem(name: string): string {
  return name.replace(/\.[^.]+$/, "") || "output";
}

// ---------------------------------------------------------------- markup

const MARKUP = `
  <div class="alert error" data-el="alert" hidden>
    <strong data-el="alertMessage"></strong>
    <span class="alert-hint" data-el="alertHint"></span>
  </div>

  <section class="card">
    <div class="row">
      <h2 class="card-title">1 · Files</h2>
      <span class="grow"></span>
      <button type="button" class="chip ghost" data-el="clearQueueBtn" hidden>Clear all</button>
    </div>

    <div class="dropzone" data-el="dropzone" role="button" tabindex="0"
         aria-label="Drop files here, or press Enter to browse">
      <svg class="dropzone-icon" viewBox="0 0 24 24" aria-hidden="true">
        <path d="M12 3v12.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>
        <circle cx="9" cy="16.5" r="3" fill="none" stroke="currentColor" stroke-width="1.6"/>
        <path d="M12 3l6 2v3l-6-2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>
      </svg>
      <p class="dropzone-title">Drop files here</p>
      <p class="dropzone-hint">or <span class="link">browse your files</span> — images, audio, video, documents, PDFs</p>
    </div>

    <div class="queue-list" data-el="queueList"></div>
    <p class="hint" data-el="queueNote"></p>
  </section>

  <section class="card" data-el="optionsCard" hidden>
    <h2 class="card-title">2 · Convert to</h2>

    <div data-el="imageOptions" hidden>
      <div class="row wrap">
        <label class="field">
          Format
          <select data-el="imageFormat"></select>
        </label>
        <label class="field" data-el="imageQualityField">
          Quality — <b data-el="imageQualityValue"></b>
          <input type="range" data-el="imageQuality" min="1" max="100" step="1" />
        </label>
      </div>
      <p class="hint">SVG inputs are rasterized at their own intrinsic size — vector data is not preserved in any of these targets.</p>
    </div>

    <div data-el="audioOptions" hidden>
      <label class="field">
        Format
        <select data-el="audioFormat"></select>
      </label>
    </div>

    <div data-el="videoOptions" hidden>
      <div class="row wrap">
        <label class="field">
          Format
          <select data-el="videoFormat"></select>
        </label>
        <label class="field">
          Quality
          <select data-el="videoQuality">
            <option value="smaller">Smaller file</option>
            <option value="balanced" selected>Balanced</option>
            <option value="sharper">Sharper</option>
          </select>
        </label>
      </div>
      <label class="field check" data-el="preferHardwareField">
        <span class="check-row">
          <input type="checkbox" data-el="preferHardware" checked />
          <span>Use hardware acceleration when available</span>
        </span>
      </label>
      <p class="hint" data-el="hardwareNote">Checking available hardware…</p>
    </div>

    <div data-el="documentOptions" hidden>
      <p class="hint" data-el="pandocMissing" hidden></p>
      <div class="row wrap">
        <label class="field">
          Format
          <select data-el="documentFormat"></select>
        </label>
      </div>
      <label class="field check" data-el="documentToPdfField">
        <span class="check-row">
          <input type="checkbox" data-el="documentToPdf" />
          <span>Convert to PDF instead (via a printed HTML page)</span>
        </span>
      </label>
      <p class="hint" data-el="documentToPdfNote"></p>
    </div>

    <div data-el="pdfOptions" hidden>
      <p class="hint" data-el="pdfiumMissing" hidden></p>
      <div class="tier-grid cols-2" data-el="pdfModeGrid" role="radiogroup" aria-label="PDF conversion mode">
        <button type="button" class="tier" data-mode="images">
          <span class="tier-name">To images</span>
          <span class="tier-detail">One image per page</span>
        </button>
        <button type="button" class="tier" data-mode="text">
          <span class="tier-name">To text</span>
          <span class="tier-detail">Best-effort extraction</span>
        </button>
      </div>
      <div class="row wrap" data-el="pdfImageRow">
        <label class="field">
          Image format
          <select data-el="pdfImageFormat">
            <option value="png">PNG</option>
            <option value="jpeg">JPEG</option>
            <option value="webp">WebP</option>
          </select>
        </label>
        <label class="field">
          DPI
          <input type="number" class="mono" data-el="pdfDpi" min="36" max="600" step="1" />
        </label>
      </div>
      <label class="field">
        Pages
        <input type="text" class="mono" data-el="pdfPages" placeholder="All pages, or e.g. 1-3,5,8-10" spellcheck="false" />
      </label>
      <p class="hint" data-el="pdfPagesHint"></p>
    </div>

    <div data-el="combineOptions" hidden>
      <label class="check">
        <input type="checkbox" data-el="combineImagesToPdf" />
        <span>Combine all <b data-el="combineCount"></b> images into one PDF instead of converting them separately</span>
      </label>
      <label class="field" data-el="combineNameField" hidden>
        Output file name
        <input type="text" class="mono" data-el="combineOutputName" spellcheck="false" />
      </label>
    </div>
  </section>

  <section class="card" data-el="progressCard" hidden>
    <div class="progress-head">
      <span class="progress-stage" data-el="progressStage">Working…</span>
      <span class="progress-pct" data-el="progressPct"></span>
    </div>
    <div class="bar" data-el="progressBar"><i></i></div>
    <div class="progress-note" data-el="progressNote"></div>
    <div class="row" style="margin-top:12px">
      <button class="danger" data-el="cancelBtn">Cancel</button>
      <button class="ghost" data-el="revealBtn" hidden>Show in folder</button>
    </div>
  </section>
`;

// ------------------------------------------------------------------ view

export class ConverterView implements Tool {
  readonly id: ToolId = "convert";

  private readonly root: HTMLElement;
  private readonly chrome: Chrome;
  private readonly state: State;
  private readonly el: Record<string, HTMLElement> = {};
  private readonly jobResolvers = new Map<string, () => void>();

  constructor(root: HTMLElement, chrome: Chrome) {
    this.root = root;
    this.chrome = chrome;

    this.state = {
      items: [],
      outputDir: "",
      separator: "/",

      imageFormat: "png",
      imageQuality: DEFAULT_JPEG_QUALITY,

      audioFormat: "mp3",

      videoFormat: "mp4",
      videoQuality: "balanced",
      preferHardware: true,

      documentFormat: "docx",
      documentToPdf: false,

      pdfMode: "images",
      pdfImageFormat: "png",
      pdfDpi: DEFAULT_DPI,
      pdfPages: "",

      combineImagesToPdf: false,
      combineOutputName: "",

      hardware: null,
      hardwareLoaded: false,

      jobId: null,
      activeIndex: null,
      cancelledBatch: false,
    };

    root.classList.add("panel");
    root.setAttribute("role", "tabpanel");
    root.innerHTML = MARKUP;

    this.populateSelects();
    this.wire();
    this.syncQueue();
    this.syncOptions();
    this.syncActions();
  }

  private pick<T extends HTMLElement>(name: string): T {
    const cached = this.el[name];
    if (cached) return cached as T;
    const node = this.root.querySelector<T>(`[data-el="${name}"]`);
    if (!node) throw new Error(`convert: missing [data-el="${name}"]`);
    this.el[name] = node;
    return node;
  }

  private select(name: string): HTMLSelectElement {
    return this.pick<HTMLSelectElement>(name);
  }

  private input(name: string): HTMLInputElement {
    return this.pick<HTMLInputElement>(name);
  }

  // ------------------------------------------------------------ tab hooks

  activate(): void {
    this.root.hidden = false;
    void this.loadHardware();
    this.syncActions();
  }

  deactivate(): void {
    this.root.hidden = true;
  }

  refresh(): void {
    this.syncActions();
  }

  get busy(): boolean {
    return this.state.jobId !== null;
  }

  // -------------------------------------------------------------- helpers

  private showError(error: ErrorPayload | null): void {
    const alert = this.pick("alert");
    if (!error) {
      alert.hidden = true;
      return;
    }
    alert.hidden = false;
    this.pick("alertMessage").textContent = error.message;
    const hint = this.pick("alertHint");
    hint.textContent = error.hint ?? "";
    hint.hidden = !error.hint;
  }

  private joinPath(dir: string, name: string): string {
    if (!dir) return name;
    return `${dir.replace(/[\\/]+$/, "")}${this.state.separator}${name}`;
  }

  private ensureDestination(): void {
    if (this.state.outputDir) return;
    this.state.outputDir = this.chrome.defaults.dir;
    this.state.separator = this.chrome.defaults.separator;
  }

  private populateSelects(): void {
    const fill = (name: string, options: ReadonlyArray<[string, string]>) => {
      const el = this.select(name);
      el.replaceChildren(
        ...options.map(([value, label]) => Object.assign(document.createElement("option"), { value, textContent: label })),
      );
    };
    fill("imageFormat", IMAGE_FORMATS);
    fill("audioFormat", AUDIO_FORMATS);
    fill("videoFormat", VIDEO_FORMATS);
    fill("documentFormat", DOC_FORMATS);

    this.select("imageFormat").value = this.state.imageFormat;
    this.select("audioFormat").value = this.state.audioFormat;
    this.select("videoFormat").value = this.state.videoFormat;
    this.select("documentFormat").value = this.state.documentFormat;
    this.input("imageQuality").value = String(this.state.imageQuality);
    this.pick("imageQualityValue").textContent = String(this.state.imageQuality);
    this.input("pdfDpi").value = String(this.state.pdfDpi);
  }

  // ---------------------------------------------------------------- queue

  private activeCategory(): Category | null {
    const known = this.state.items.filter((i) => i.category !== "unknown");
    const first = known[0]?.category;
    if (!first) return null;
    return known.every((i) => i.category === first) ? first : null;
  }

  private async addFiles(paths: string[]): Promise<void> {
    this.ensureDestination();
    for (const path of paths) {
      if (this.state.items.some((i) => i.path === path)) continue;

      let category: Category = "unknown";
      let pageCount: number | null = null;
      try {
        const inspection = await api.convertInspect(path);
        category = inspection.category;
        pageCount = inspection.pageCount;
      } catch {
        // Falls back to "unknown" — the row shows it as unsupported rather
        // than silently dropping the file the user just added.
      }

      this.state.items.push({ path, name: baseName(path), category, pageCount, status: "queued" });
    }
    this.showError(null);
    this.syncQueue();
    this.syncOptions();
    this.syncActions();
  }

  private removeItem(path: string): void {
    this.state.items = this.state.items.filter((i) => i.path !== path);
    this.syncQueue();
    this.syncOptions();
    this.syncActions();
  }

  private clearQueue(): void {
    if (this.busy) return;
    this.state.items = [];
    this.syncQueue();
    this.syncOptions();
    this.syncActions();
  }

  private syncQueue(): void {
    const list = this.pick("queueList");
    list.replaceChildren(
      ...this.state.items.map((item) => {
        const row = document.createElement("div");
        row.className = `queue-row queue-${item.status}`;

        const name = document.createElement("span");
        name.className = "queue-name";
        name.textContent = item.name;

        const meta = document.createElement("span");
        meta.className = "queue-meta";
        meta.textContent =
          item.category === "unknown"
            ? "Unsupported format"
            : item.category === "pdf" && item.pageCount !== null
              ? `PDF · ${item.pageCount} page${item.pageCount === 1 ? "" : "s"}`
              : CATEGORY_LABEL[item.category];

        const status = document.createElement("span");
        status.className = "queue-status";
        status.textContent =
          item.status === "queued"
            ? ""
            : item.status === "converting"
              ? "Converting…"
              : item.status === "done"
                ? "Done"
                : item.status === "skipped"
                  ? "Skipped"
                  : (item.message ?? "Failed");

        const remove = document.createElement("button");
        remove.type = "button";
        remove.className = "chip ghost queue-remove";
        remove.textContent = "×";
        remove.setAttribute("aria-label", `Remove ${item.name}`);
        remove.disabled = this.busy;
        remove.addEventListener("click", () => this.removeItem(item.path));

        row.append(name, meta, status, remove);
        return row;
      }),
    );

    this.pick<HTMLButtonElement>("clearQueueBtn").hidden = this.state.items.length === 0;

    const note = this.pick("queueNote");
    const category = this.activeCategory();
    const hasUnknown = this.state.items.some((i) => i.category === "unknown");

    if (this.state.items.length === 0) {
      note.textContent = "";
    } else if (hasUnknown) {
      note.textContent = "Remove the unsupported file(s) above to continue.";
    } else if (category === null) {
      note.textContent = "Mixed file types queued — everything in one batch must be the same kind (e.g. all images, or all documents).";
    } else {
      note.textContent = "";
    }
  }

  // -------------------------------------------------------------- options

  private syncOptions(): void {
    const category = this.activeCategory();
    this.pick("optionsCard").hidden = category === null;

    for (const name of ["imageOptions", "audioOptions", "videoOptions", "documentOptions", "pdfOptions"]) {
      this.pick(name).hidden = true;
    }

    if (category === "image") this.pick("imageOptions").hidden = false;
    if (category === "audio") this.pick("audioOptions").hidden = false;
    if (category === "video") this.pick("videoOptions").hidden = false;
    if (category === "document") this.pick("documentOptions").hidden = false;
    if (category === "pdf") this.pick("pdfOptions").hidden = false;

    this.pick("imageQualityField").hidden = this.state.imageFormat !== "jpeg";

    const combineEligible = category === "image" && this.state.items.length >= 2;
    this.pick("combineOptions").hidden = !combineEligible;
    if (!combineEligible) this.state.combineImagesToPdf = false;
    this.pick("combineCount").textContent = String(this.state.items.length);
    this.pick("combineNameField").hidden = !this.state.combineImagesToPdf;
    if (this.state.combineImagesToPdf && !this.state.combineOutputName) {
      this.state.combineOutputName = `${stem(this.state.items[0]?.name ?? "combined")}.pdf`;
      this.input("combineOutputName").value = this.state.combineOutputName;
    }

    this.syncPdfMode();
    this.syncDocumentAvailability();
  }

  private syncPdfMode(): void {
    const missing = this.state.hardwareLoaded && this.state.hardware?.pdfAvailable === false;

    for (const button of this.pick("pdfModeGrid").querySelectorAll<HTMLButtonElement>("[data-mode]")) {
      const active = button.dataset.mode === this.state.pdfMode;
      button.classList.toggle("active", active);
      button.setAttribute("aria-checked", String(active));
      // Mirrors the accuracy-tier grid in transcribe.ts: grey out and disable
      // the specific option that needs a missing tool, rather than the tab.
      button.disabled = missing;
      button.setAttribute("data-unavailable", String(missing));
    }
    this.pick("pdfImageRow").hidden = this.state.pdfMode !== "images";

    const pdfs = this.state.items.filter((i) => i.category === "pdf");
    const pageCount = pdfs.length === 1 ? pdfs[0]?.pageCount : undefined;
    if (pageCount) {
      this.pick("pdfPagesHint").textContent = `This PDF has ${pageCount} page${pageCount === 1 ? "" : "s"}. Leave blank for all of them.`;
    } else {
      this.pick("pdfPagesHint").textContent = "Applies to every queued PDF. Leave blank for every page.";
    }

    this.pick("pdfiumMissing").hidden = !missing;
    if (missing) {
      this.pick("pdfiumMissing").textContent =
        "PDFium was not found, so PDF conversion is unavailable. Run `npm run pdfium` to vendor it.";
    }
  }

  private syncDocumentAvailability(): void {
    const loaded = this.state.hardwareLoaded;
    const pandocMissing = loaded && this.state.hardware?.documentsAvailable === false;
    this.pick("pandocMissing").hidden = !pandocMissing;
    if (pandocMissing) {
      this.pick("pandocMissing").textContent = "Pandoc was not found, so document conversion is unavailable. Run `npm run pandoc` to vendor it.";
    }

    const toPdfAvailable = this.state.hardware?.documentsToPdfAvailable ?? false;
    const toPdfCheckbox = this.input("documentToPdf");
    toPdfCheckbox.disabled = !toPdfAvailable;
    this.pick("documentToPdfField").classList.toggle("disabled", !toPdfAvailable);
    this.pick("documentToPdfNote").textContent = !loaded
      ? ""
      : toPdfAvailable
        ? "Renders through wkhtmltopdf, which was found on this machine."
        : "Needs wkhtmltopdf on PATH — it isn't vendored (its upstream stopped publishing Windows/macOS builds), so this only works if you've installed it yourself.";
    if (!toPdfAvailable) this.state.documentToPdf = false;
  }

  private async loadHardware(): Promise<void> {
    if (this.state.hardwareLoaded) return;
    try {
      this.state.hardware = await api.convertHardwareReport();
    } catch {
      this.state.hardware = null;
    }
    this.state.hardwareLoaded = true;

    const note = this.pick("hardwareNote");
    if (this.state.hardware) {
      const { videoEncoder, videoEncoderIsHardware } = this.state.hardware;
      note.textContent = videoEncoderIsHardware
        ? `Hardware acceleration detected: ${videoEncoder}. Falls back to the CPU automatically if it ever fails mid-render.`
        : `No hardware encoder detected — video will be encoded on the CPU (${videoEncoder}).`;
    } else {
      note.textContent = "Could not check for hardware acceleration; video will be encoded on the CPU.";
    }

    this.syncOptions();
    this.syncActions();
  }

  // -------------------------------------------------------------- options

  private buildOptions(): ConvertOptions | null {
    const category = this.activeCategory();

    if (this.state.combineImagesToPdf && category === "image" && this.state.items.length >= 2) {
      return { kind: "imagesToPdf" };
    }

    switch (category) {
      case "image":
        return {
          kind: "image",
          format: this.state.imageFormat,
          quality: this.state.imageFormat === "jpeg" ? this.state.imageQuality : null,
        };
      case "audio":
        return { kind: "audio", format: this.state.audioFormat };
      case "video":
        return {
          kind: "video",
          format: this.state.videoFormat,
          quality: this.state.videoQuality,
          preferHardware: this.state.preferHardware,
        };
      case "document":
        return this.state.documentToPdf
          ? { kind: "documentToPdf" }
          : { kind: "document", format: this.state.documentFormat };
      case "pdf":
        return this.state.pdfMode === "images"
          ? {
              kind: "pdfToImages",
              format: this.state.pdfImageFormat,
              dpi: this.state.pdfDpi,
              pages: this.state.pdfPages.trim() || null,
            }
          : { kind: "pdfToText", pages: this.state.pdfPages.trim() || null };
      default:
        return null;
    }
  }

  // -------------------------------------------------------------- actions

  private syncActions(): void {
    const category = this.activeCategory();
    const hasUnknown = this.state.items.some((i) => i.category === "unknown");
    const options = this.busy ? null : this.buildOptions();

    // Unlike the platform tabs, this one does not gate on `chrome.ready()`:
    // that reflects yt-dlp/ffmpeg/ffprobe, and most conversions here (images,
    // documents, PDF) need none of them. A missing FFmpeg only matters for
    // Audio/Video, and `start_conversion` reports that itself, synchronously,
    // the moment such a job is actually requested.
    const ready =
      this.state.items.length > 0 && category !== null && !hasUnknown && this.state.outputDir !== "" && options !== null;

    const combining = options?.kind === "imagesToPdf";
    const count = this.state.items.length;

    this.chrome.setActions(this, {
      canChoose: !this.busy,
      canRender: ready && !this.busy,
      busy: this.busy,
      path: this.state.outputDir ? `Output folder: ${this.state.outputDir}` : "No output folder selected",
      chooseLabel: "Choose folder…",
      renderLabel: combining ? "Combine into PDF" : count > 1 ? `Convert ${count} files` : "Convert",
      busyLabel: combining ? "Combining…" : "Converting…",
    });
  }

  async chooseDestination(): Promise<void> {
    const chosen = await open({ directory: true, multiple: false, title: "Choose an output folder" });
    if (typeof chosen !== "string") return;
    this.state.outputDir = chosen;
    this.state.separator = chosen.includes("\\") ? "\\" : "/";
    this.syncActions();
  }

  // ----------------------------------------------------------- conversion

  private resetProgress(stage: string): void {
    this.pick("progressCard").hidden = false;
    this.pick("progressStage").textContent = stage;
    this.pick("progressPct").textContent = "";
    const bar = this.pick("progressBar");
    bar.className = "bar indeterminate";
    (bar.firstElementChild as HTMLElement).style.width = "0%";
    this.pick("progressNote").textContent = "";
    this.pick("cancelBtn").hidden = false;
    this.pick("revealBtn").hidden = true;
  }

  private waitForJob(jobId: string): Promise<void> {
    return new Promise((resolve) => this.jobResolvers.set(jobId, resolve));
  }

  async startRender(): Promise<void> {
    if (this.busy) return;
    const options = this.buildOptions();
    if (!options) return;

    this.showError(null);
    this.state.cancelledBatch = false;

    if (options.kind === "imagesToPdf") {
      await this.runCombine(options);
    } else {
      await this.runBatch(options);
    }
  }

  private async runCombine(options: ConvertOptions): Promise<void> {
    let extension: string;
    try {
      extension = await api.convertTargetExtension(options);
    } catch (raw) {
      this.showError(toErrorPayload(raw));
      return;
    }

    const rawName = this.state.combineOutputName.trim() || `combined.${extension}`;
    const name = rawName.toLowerCase().endsWith(`.${extension}`) ? rawName : `${rawName}.${extension}`;
    const outputPath = this.joinPath(this.state.outputDir, name);

    this.state.activeIndex = -1;
    this.resetProgress(`Combining ${this.state.items.length} images…`);

    try {
      const jobId = await api.startConversion({
        inputPaths: this.state.items.map((i) => i.path),
        outputPath,
        options,
      });
      this.state.jobId = jobId;
      this.syncActions();
      await this.waitForJob(jobId);
      this.pick("cancelBtn").hidden = true;
      this.pick("revealBtn").hidden = false;
    } catch (raw) {
      // The job never actually started (a synchronous validation failure),
      // so there is no progress to leave on screen.
      this.pick("progressCard").hidden = true;
      this.showError(toErrorPayload(raw));
    }

    this.state.jobId = null;
    this.state.activeIndex = null;
    this.syncActions();
  }

  private async runBatch(options: ConvertOptions): Promise<void> {
    let extension: string;
    try {
      extension = await api.convertTargetExtension(options);
    } catch (raw) {
      this.showError(toErrorPayload(raw));
      return;
    }

    for (const [i, item] of this.state.items.entries()) {
      if (this.state.cancelledBatch) break;
      if (item.category === "unknown") continue;

      item.status = "converting";
      this.state.activeIndex = i;
      this.syncQueue();
      this.resetProgress(`Converting ${item.name} (${i + 1} of ${this.state.items.length})…`);

      const outputPath = this.joinPath(this.state.outputDir, `${stem(item.name)}.${extension}`);

      try {
        const jobId = await api.startConversion({ inputPaths: [item.path], outputPath, options });
        this.state.jobId = jobId;
        this.syncActions();
        await this.waitForJob(jobId);
      } catch (raw) {
        item.status = "failed";
        item.message = toErrorPayload(raw).message;
        this.showError(toErrorPayload(raw));
      }
      this.syncQueue();
    }

    this.state.jobId = null;
    this.state.activeIndex = null;
    this.pick("cancelBtn").hidden = true;
    this.pick("revealBtn").hidden = false;
    this.syncActions();
  }

  /** Backend job progress. Only reacts to the job this view is currently running. */
  handleProgress(event: JobEvent): void {
    if (event.jobId !== this.state.jobId) return;

    const bar = this.pick("progressBar");
    const determinate = event.percent !== null;
    bar.className = `bar${determinate ? "" : " indeterminate"}`;
    if (determinate) {
      const pct = event.percent ?? 0;
      (bar.firstElementChild as HTMLElement).style.width = `${pct}%`;
      this.pick("progressPct").textContent = `${pct.toFixed(0)}%`;
    }

    if (event.stage !== "done" && event.stage !== "failed" && event.stage !== "cancelled") {
      this.pick("progressNote").textContent = event.message;
      return;
    }

    const index = this.state.activeIndex;
    const outcome: ItemStatus = event.stage === "done" ? "done" : event.stage === "cancelled" ? "skipped" : "failed";
    const message = event.stage === "failed" ? (event.error?.message ?? event.message) : undefined;

    if (index === -1) {
      // Combining images into one PDF touches every queued item at once —
      // there is no single row this result belongs to.
      for (const item of this.state.items) {
        item.status = outcome;
        item.message = message;
      }
    } else if (index !== null) {
      const item = this.state.items[index];
      if (item) {
        item.status = outcome;
        item.message = message;
      }
    }
    bar.className = `bar ${event.stage === "done" ? "done" : "failed"}`;
    this.syncQueue();

    const resolve = this.jobResolvers.get(event.jobId);
    this.jobResolvers.delete(event.jobId);
    resolve?.();
  }

  /** Called by the shell when a conversion summary arrives. */
  acceptSummary(summary: ConvertSummary): void {
    this.pick("progressNote").textContent =
      summary.outputPaths.length > 1 ? `${summary.note} — ${summary.outputPaths.length} files written.` : summary.note;
  }

  // ---------------------------------------------------------------- wiring

  private async browse(): Promise<void> {
    if (this.busy) return;
    const chosen = await open({
      multiple: true,
      directory: false,
      title: "Choose files to convert",
      filters: [
        { name: "All supported", extensions: ALL_EXT },
        { name: "Images", extensions: IMAGE_EXT },
        { name: "Audio", extensions: AUDIO_EXT },
        { name: "Video", extensions: VIDEO_EXT },
        { name: "Documents", extensions: DOC_EXT },
        { name: "PDF", extensions: ["pdf"] },
      ],
    });
    if (!chosen) return;
    await this.addFiles(Array.isArray(chosen) ? chosen : [chosen]);
  }

  private wire(): void {
    const dropzone = this.pick("dropzone");
    dropzone.addEventListener("click", () => void this.browse());
    dropzone.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        void this.browse();
      }
    });

    let depth = 0;
    dropzone.addEventListener("dragenter", (event) => {
      event.preventDefault();
      depth += 1;
      dropzone.classList.add("over");
    });
    dropzone.addEventListener("dragover", (event) => {
      event.preventDefault();
      if (event.dataTransfer) event.dataTransfer.dropEffect = "copy";
    });
    dropzone.addEventListener("dragleave", () => {
      depth = Math.max(0, depth - 1);
      if (depth === 0) dropzone.classList.remove("over");
    });
    dropzone.addEventListener("drop", (event) => {
      event.preventDefault();
      depth = 0;
      dropzone.classList.remove("over");
      // Dropped `File` objects carry no filesystem path in the webview, so a
      // drop still opens the picker — exactly the trade-off `transcribe.ts`
      // documents for the same reason.
      if (event.dataTransfer?.files.length) void this.browse();
    });
    for (const type of ["dragover", "drop"] as const) {
      this.root.addEventListener(type, (event) => {
        if (!(event.target as HTMLElement).closest('[data-el="dropzone"]')) event.preventDefault();
      });
    }

    this.pick("clearQueueBtn").addEventListener("click", () => this.clearQueue());

    this.select("imageFormat").addEventListener("change", () => {
      this.state.imageFormat = this.select("imageFormat").value as ImageFormat;
      this.syncOptions();
    });
    this.input("imageQuality").addEventListener("input", () => {
      this.state.imageQuality = Number(this.input("imageQuality").value);
      this.pick("imageQualityValue").textContent = String(this.state.imageQuality);
    });
    this.select("audioFormat").addEventListener("change", () => {
      this.state.audioFormat = this.select("audioFormat").value as AudioFormat;
    });
    this.select("videoFormat").addEventListener("change", () => {
      this.state.videoFormat = this.select("videoFormat").value as VideoFormat;
    });
    this.select("videoQuality").addEventListener("change", () => {
      this.state.videoQuality = this.select("videoQuality").value as VideoQuality;
    });
    this.input("preferHardware").addEventListener("change", () => {
      this.state.preferHardware = this.input("preferHardware").checked;
    });
    this.select("documentFormat").addEventListener("change", () => {
      this.state.documentFormat = this.select("documentFormat").value as DocFormat;
    });
    this.input("documentToPdf").addEventListener("change", () => {
      this.state.documentToPdf = this.input("documentToPdf").checked;
      this.syncActions();
    });

    this.pick("pdfModeGrid").addEventListener("click", (event) => {
      const button = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-mode]");
      const mode = button?.dataset.mode;
      if (mode === "images" || mode === "text") {
        this.state.pdfMode = mode;
        this.syncOptions();
        this.syncActions();
      }
    });
    this.select("pdfImageFormat").addEventListener("change", () => {
      this.state.pdfImageFormat = this.select("pdfImageFormat").value as ImageFormat;
    });
    this.input("pdfDpi").addEventListener("input", () => {
      const value = Number(this.input("pdfDpi").value);
      this.state.pdfDpi = Number.isFinite(value) ? value : DEFAULT_DPI;
    });
    this.input("pdfPages").addEventListener("input", () => {
      this.state.pdfPages = this.input("pdfPages").value;
    });

    this.input("combineImagesToPdf").addEventListener("change", () => {
      this.state.combineImagesToPdf = this.input("combineImagesToPdf").checked;
      this.syncOptions();
      this.syncActions();
    });
    this.input("combineOutputName").addEventListener("input", () => {
      this.state.combineOutputName = this.input("combineOutputName").value;
    });

    this.pick("cancelBtn").addEventListener("click", () => {
      this.state.cancelledBatch = true;
      if (this.state.jobId) void api.cancelJob(this.state.jobId);
    });
    this.pick("revealBtn").addEventListener("click", () => {
      if (this.state.outputDir) void api.revealInFolder(this.state.outputDir);
    });
  }
}
