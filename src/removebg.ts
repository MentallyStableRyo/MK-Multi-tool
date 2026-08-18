/**
 * The Remove Background workspace.
 *
 * Structurally a sibling of {@link PlatformView}: it owns its panel's DOM, keeps
 * its state whether or not it is on screen, and borrows the shell's action bar
 * while active. It differs in that it starts no backend job — all the work
 * happens in {@link module:matte} inside this window — so the action bar's two
 * buttons mean "pick a destination" and "write the PNG" rather than "save to"
 * and "render".
 *
 * The user drops or picks an image and the cut-out starts immediately; there is
 * no third step to press. Everything after that is looking at the result and
 * deciding where to put it.
 */

import { save } from "@tauri-apps/plugin-dialog";

import { api, toErrorPayload } from "./bridge";
import {
  MatteError,
  cutout,
  decode,
  isLoaded,
  tiers,
  toPng,
  type Matte,
  type Progress,
  type Tier,
  type TierSpec,
} from "./matte";
import type { Actions, Chrome, Tool, ToolId } from "./tool";
import type { ErrorPayload } from "./types";

interface State {
  file: File | null;
  /** Kept alongside the matte so the "before" image needs no second decode. */
  bitmap: ImageBitmap | null;
  matte: Matte | null;
  showing: "original" | "cutout";
  /** Which model the next run uses. Resolved from the manifest at first use. */
  tier: Tier;
  processing: boolean;
  exporting: boolean;
  outputDir: string;
  outputName: string;
  separator: string;
  savedTo: string | null;
}

const ACCEPT = "image/png,image/jpeg,image/webp,image/bmp,image/gif";

/** Turn `holiday photo.JPG` into `holiday photo (cut-out).png`. */
function outputNameFor(fileName: string): string {
  const stem = fileName.replace(/\.[^./\\]+$/, "") || "image";
  return `${stem} (cut-out).png`;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(value < 10 ? 1 : 0)} ${units[unit]}`;
}

// ---------------------------------------------------------------- markup

const MARKUP = `
  <div class="alert error" data-el="alert" hidden>
    <strong data-el="alertMessage"></strong>
    <span class="alert-hint" data-el="alertHint"></span>
  </div>

  <section class="card">
    <h2 class="card-title">1 · Image</h2>

    <div class="dropzone" data-el="dropzone" role="button" tabindex="0"
         aria-label="Drop an image here, or press Enter to browse">
      <svg class="dropzone-icon" viewBox="0 0 24 24" aria-hidden="true">
        <path d="M4 15.5V17a3 3 0 0 0 3 3h10a3 3 0 0 0 3-3v-1.5" fill="none"
              stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>
        <path d="M12 15V4m0 0L8.2 7.8M12 4l3.8 3.8" fill="none" stroke="currentColor"
              stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>
      </svg>
      <p class="dropzone-title">Drop an image here</p>
      <p class="dropzone-hint">or <span class="link">browse your files</span> — PNG, JPG, or WEBP</p>
    </div>

    <input type="file" data-el="fileInput" accept="${ACCEPT}" hidden />

    <div class="row file-line" data-el="fileLine" hidden>
      <span class="file-name" data-el="fileName"></span>
      <span class="grow"></span>
      <span class="hint" data-el="fileMeta"></span>
      <button class="chip" data-el="replaceBtn">Replace</button>
    </div>
  </section>

  <section class="card" data-el="progressCard" hidden>
    <div class="progress-head">
      <span class="progress-stage" data-el="progressStage">Working…</span>
      <span class="grow"></span>
      <span class="progress-pct" data-el="progressPct"></span>
    </div>
    <div class="bar" data-el="progressBar"><i></i></div>
    <div class="progress-note" data-el="progressNote"></div>
    <div class="row" style="margin-top:12px">
      <button class="danger" data-el="cancelBtn">Cancel</button>
    </div>
  </section>

  <section class="card" data-el="resultCard" hidden>
    <h2 class="card-title">2 · Result</h2>

    <div class="preview-bar">
      <div class="segmented" role="group" aria-label="Compare">
        <button class="chip" data-el="showOriginal">Original</button>
        <button class="chip accent" data-el="showCutout">Cut-out</button>
      </div>
      <span class="grow"></span>
      <div class="segmented" role="group" aria-label="Quality" data-el="qualityGroup" hidden></div>
      <span class="hint" data-el="resultMeta"></span>
    </div>

    <div class="checker-stage" data-el="stage">
      <img class="compare-layer" data-el="originalImage" alt="The image before the background was removed" hidden />
      <div class="compare-layer" data-el="cutoutSlot"></div>
    </div>

    <p class="hint" data-el="resultNote"></p>
  </section>
`;

// ------------------------------------------------------------------ view

export class RemoveBackgroundView implements Tool {
  readonly id: ToolId = "removebg";

  private readonly root: HTMLElement;
  private readonly chrome: Chrome;
  private readonly state: State;
  private readonly el: Record<string, HTMLElement> = {};

  /** Object URL for the "before" image; revoked whenever it is replaced. */
  private originalUrl: string | null = null;

  /** The quality buttons are built from the manifest, once. */
  private qualityBuilt = false;

  /**
   * Guards against a stale run finishing after the user has already dropped a
   * different image — only the newest run is allowed to touch the UI.
   */
  private generation = 0;

  constructor(root: HTMLElement, chrome: Chrome) {
    this.root = root;
    this.chrome = chrome;

    this.state = {
      file: null,
      bitmap: null,
      matte: null,
      showing: "cutout",
      // Overwritten from the manifest the first time the tab is opened; the
      // manifest is authoritative about which tiers were actually vendored.
      tier: "precise",
      processing: false,
      exporting: false,
      outputDir: "",
      outputName: "",
      separator: "/",
      savedTo: null,
    };

    root.classList.add("panel");
    root.setAttribute("role", "tabpanel");
    root.innerHTML = MARKUP;

    this.wire();
    this.syncActions();
  }

  // --------------------------------------------------------- element refs

  private pick<T extends HTMLElement>(name: string): T {
    const cached = this.el[name];
    if (cached) return cached as T;

    const node = this.root.querySelector<T>(`[data-el="${name}"]`);
    if (!node) throw new Error(`removebg: missing [data-el="${name}"]`);
    this.el[name] = node;
    return node;
  }

  // ------------------------------------------------------------ tab hooks

  activate(): void {
    this.root.hidden = false;
    this.syncActions();
    // Opening the tab is the first point at which the vendored tiers matter.
    // This reads the manifest only — a few hundred bytes, no runtime, no model.
    void this.syncQualityOptions();
    if (!this.state.file) this.pick("dropzone").focus();
  }

  deactivate(): void {
    this.root.hidden = true;
  }

  refresh(): void {
    this.syncActions();
  }

  get busy(): boolean {
    return this.state.processing || this.state.exporting;
  }

  // --------------------------------------------------------------- wiring

  private wire(): void {
    const dropzone = this.pick("dropzone");
    const fileInput = this.pick<HTMLInputElement>("fileInput");

    this.pick("cancelBtn").addEventListener("click", () => this.cancelRun());

    dropzone.addEventListener("click", () => fileInput.click());
    dropzone.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        fileInput.click();
      }
    });

    this.pick("replaceBtn").addEventListener("click", () => fileInput.click());

    fileInput.addEventListener("change", () => {
      const file = fileInput.files?.[0];
      // Reset so re-picking the same file still fires a change event.
      fileInput.value = "";
      if (file) void this.accept(file);
    });

    // Tauri's own drag-drop handler is disabled in tauri.conf.json, so these are
    // ordinary HTML5 events carrying real File objects rather than paths.
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
    // dragleave fires for every child crossed, so it is counted rather than
    // trusted — otherwise the highlight flickers off over the icon and text.
    dropzone.addEventListener("dragleave", () => {
      depth = Math.max(0, depth - 1);
      if (depth === 0) dropzone.classList.remove("over");
    });
    dropzone.addEventListener("drop", (event) => {
      event.preventDefault();
      depth = 0;
      dropzone.classList.remove("over");

      const file = event.dataTransfer?.files?.[0];
      if (file) void this.accept(file);
    });

    // A drop that misses the zone would otherwise navigate the webview to the
    // file, replacing the app with a picture of a cat.
    for (const type of ["dragover", "drop"] as const) {
      this.root.addEventListener(type, (event) => {
        if (!(event.target as HTMLElement).closest('[data-el="dropzone"]')) {
          event.preventDefault();
        }
      });
    }

    this.pick("showOriginal").addEventListener("click", () => this.show("original"));
    this.pick("showCutout").addEventListener("click", () => this.show("cutout"));

    // The quality buttons are built from the manifest, so they are delegated
    // rather than bound individually.
    this.pick("qualityGroup").addEventListener("click", (event) => {
      const button = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-tier]");
      const tier = button?.dataset.tier as Tier | undefined;
      if (tier) void this.chooseTier(tier);
    });
  }

  // ------------------------------------------------------------- quality

  /** Draw one button per vendored tier. A lone tier gets no toggle at all. */
  private async syncQualityOptions(): Promise<void> {
    if (this.qualityBuilt) return;

    let available: Array<[Tier, TierSpec]>;
    let fallback: Tier;
    try {
      const resolved = await tiers();
      available = resolved.available;
      fallback = resolved.default;
    } catch {
      // A missing or unreadable manifest is reported properly when the user
      // actually drops an image; there is nothing useful to say about it here.
      return;
    }

    this.qualityBuilt = true;
    this.state.tier = fallback;

    const group = this.pick("qualityGroup");
    group.hidden = available.length < 2;
    group.replaceChildren(
      ...available.map(([tier, spec]) => {
        const button = document.createElement("button");
        button.className = "chip";
        button.dataset.tier = tier;
        button.textContent = spec.label;
        button.title = spec.detail;
        return button;
      }),
    );

    this.syncQualitySelection();
  }

  private syncQualitySelection(): void {
    for (const button of this.pick("qualityGroup").querySelectorAll<HTMLButtonElement>(
      "[data-tier]",
    )) {
      button.classList.toggle("accent", button.dataset.tier === this.state.tier);
      button.disabled = this.busy;
    }
  }

  /** Switch model and re-cut the image already loaded. */
  private async chooseTier(tier: Tier): Promise<void> {
    if (tier === this.state.tier || this.busy) return;

    this.state.tier = tier;
    this.syncQualitySelection();

    // Nothing loaded yet: the choice simply applies to the next image.
    if (!this.state.bitmap) return;
    await this.run({ keepResult: true });
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

  private toPayload(error: unknown): ErrorPayload {
    if (error instanceof MatteError) {
      return { kind: "matte", message: error.message, hint: error.hint };
    }
    return toErrorPayload(error);
  }

  private setProgress(progress: Progress | null): void {
    const card = this.pick("progressCard");
    if (!progress) {
      card.hidden = true;
      return;
    }

    card.hidden = false;
    this.pick("progressStage").textContent = progress.message;
    this.pick("progressPct").textContent =
      progress.percent === null ? "" : `${Math.round(progress.percent)}%`;

    const bar = this.pick("progressBar");
    const fill = bar.firstElementChild as HTMLElement | null;
    // An indeterminate step (the one-off model load) gets a sweeping bar rather
    // than a number it cannot honestly report.
    bar.classList.toggle("indeterminate", progress.percent === null);
    if (fill && progress.percent !== null) fill.style.width = `${progress.percent}%`;
  }

  // ------------------------------------------------------------ the flow

  /** Take a file from the picker or a drop, and cut it out. */
  private async accept(file: File): Promise<void> {
    if (this.busy) return;

    // Make sure the tier is resolved from the manifest before the first cut, so
    // a drop onto a freshly opened tab still uses the default the manifest names.
    await this.syncQualityOptions();

    this.state.file = file;
    this.state.savedTo = null;

    // The name is proposed from the source file, but never overwrites a folder
    // the user has already chosen this session.
    this.state.outputName = outputNameFor(file.name);
    if (!this.state.outputDir) {
      this.state.outputDir = this.chrome.defaults.dir;
      this.state.separator = this.chrome.defaults.separator;
    }

    this.pick("fileLine").hidden = false;
    this.pick("fileName").textContent = file.name;
    this.pick("fileMeta").textContent = formatBytes(file.size);

    await this.run({ keepResult: false, decodeFrom: file });
  }

  /**
   * Cut out the loaded image at the current tier.
   *
   * `keepResult` leaves the previous cut-out on screen while a new one is
   * computed. That is what a quality switch wants — the whole point is comparing
   * the two — whereas a freshly dropped file should not sit under the last
   * image's result.
   */
  /**
   * Abandon the in-flight cut.
   *
   * Bumping the generation is the whole mechanism: `run` reads it at every
   * boundary and discards its own result once it no longer matches. An
   * inference already inside the WASM call still runs to completion — it
   * cannot be interrupted from here — but nothing waits on it, so the tab is
   * usable again immediately.
   */
  private cancelRun(): void {
    if (!this.state.processing) return;
    this.generation += 1;
    this.state.processing = false;
    this.setProgress(null);
    this.syncActions();
  }

  private async run(options: { keepResult: boolean; decodeFrom?: File }): Promise<void> {
    const run = (this.generation += 1);

    this.showError(null);
    this.state.processing = true;
    if (!options.keepResult) {
      this.state.matte = null;
      this.pick("resultCard").hidden = true;
    }
    this.syncActions();

    // A model is only read the first time it is used; say so, because that run
    // is the slow one and silence would read as a hang.
    const loaded = isLoaded(this.state.tier);
    this.setProgress({
      phase: "runtime",
      message: loaded ? "Preparing the image…" : "Loading the segmentation model…",
      percent: loaded ? 10 : null,
    });

    try {
      if (options.decodeFrom) {
        const bitmap = await decode(options.decodeFrom);
        if (run !== this.generation) {
          bitmap.close();
          return;
        }
        this.state.bitmap?.close();
        this.state.bitmap = bitmap;
        this.showOriginalImage(options.decodeFrom, bitmap);
      }

      const bitmap = this.state.bitmap;
      if (!bitmap) return;

      const matte = await cutout(
        bitmap,
        this.state.tier,
        (progress) => {
          if (run === this.generation) this.setProgress(progress);
        },
        // Cancelling bumps the generation, which is already how a superseded
        // run is abandoned — so cancel and "the user changed tier mid-run"
        // reduce to the same signal rather than two competing ones.
        () => run !== this.generation,
      );
      if (run !== this.generation) return;

      this.state.matte = matte;
      this.presentResult(matte);
      this.show("cutout");
    } catch (error) {
      if (run !== this.generation) return;
      this.showError(this.toPayload(error));
      // A failed re-cut leaves the previous result standing; a failed first cut
      // has nothing to fall back to.
      if (!options.keepResult) {
        this.state.file = null;
        this.state.matte = null;
        this.pick("fileLine").hidden = true;
      }
    } finally {
      if (run === this.generation) {
        this.state.processing = false;
        this.setProgress(null);
        this.syncActions();
      }
    }
  }

  private showOriginalImage(file: File, bitmap: ImageBitmap): void {
    if (this.originalUrl) URL.revokeObjectURL(this.originalUrl);
    this.originalUrl = URL.createObjectURL(file);

    const image = this.pick<HTMLImageElement>("originalImage");
    image.src = this.originalUrl;
    // Both layers are absolutely positioned, so the stage takes its height from
    // the aspect ratio rather than from whichever layer happens to be visible.
    this.pick("stage").style.aspectRatio = `${bitmap.width} / ${bitmap.height}`;
  }

  private presentResult(matte: Matte): void {
    const slot = this.pick("cutoutSlot");
    slot.replaceChildren(matte.canvas);
    matte.canvas.className = "cutout-canvas";

    this.pick("resultCard").hidden = false;
    // The mask resolution is the number that explains the result, so it is
    // shown rather than left for the user to infer from the quality label.
    this.pick("resultMeta").textContent =
      `${matte.width} × ${matte.height} · ${matte.maskSize}² mask · ` +
      `${(matte.inferenceMs / 1000).toFixed(1)} s`;

    const note = this.pick("resultNote");
    note.textContent = matte.scaledFrom
      ? `Scaled down from ${matte.scaledFrom.width} × ${matte.scaledFrom.height} — ` +
        `the mask carries no more detail than this, and the full size would cost far more memory.`
      : `Transparency is shown as a checkerboard; it is not part of the exported file.`;
  }

  private show(which: "original" | "cutout"): void {
    this.state.showing = which;

    this.pick("originalImage").hidden = which !== "original";
    this.pick("cutoutSlot").hidden = which !== "cutout";

    this.pick("showOriginal").classList.toggle("accent", which === "original");
    this.pick("showCutout").classList.toggle("accent", which === "cutout");
    // The checkerboard is what transparency looks like; behind an opaque
    // original it is just noise.
    this.pick("stage").classList.toggle("opaque", which === "original");
  }

  // -------------------------------------------------------- the action bar

  private currentOutputPath(): string {
    const { outputDir, outputName, separator } = this.state;
    if (!outputName) return "";
    return outputDir ? `${outputDir}${separator}${outputName}` : outputName;
  }

  private syncActions(): void {
    const ready = this.state.matte !== null;
    this.syncQualitySelection();

    const actions: Actions = {
      canChoose: ready && !this.busy,
      canRender: ready && !this.busy && this.state.outputName !== "",
      busy: this.busy,
      path: this.state.savedTo
        ? `Saved to ${this.state.savedTo}`
        : ready
          ? this.currentOutputPath()
          : "No image loaded",
      chooseLabel: "Save to…",
      renderLabel: "Export PNG",
      busyLabel: this.state.exporting ? "Exporting…" : "Cutting out…",
    };

    this.chrome.setActions(this, actions);
  }

  async chooseDestination(): Promise<void> {
    const chosen = await save({
      defaultPath: this.currentOutputPath(),
      filters: [{ name: "PNG", extensions: ["png"] }],
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

    this.state.savedTo = null;
    this.syncActions();
  }

  async startRender(): Promise<void> {
    const { matte } = this.state;
    if (!matte || this.busy) return;

    this.showError(null);
    this.state.exporting = true;
    this.syncActions();

    try {
      const png = await toPng(matte.canvas);
      const path = this.currentOutputPath();
      await api.writeFile(path, png);

      this.state.savedTo = path;
    } catch (error) {
      this.showError(this.toPayload(error));
    } finally {
      this.state.exporting = false;
      this.syncActions();
    }
  }
}
