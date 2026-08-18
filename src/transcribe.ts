/**
 * The Audio → SRT workspace.
 *
 * Unlike Remove Background, the work here happens in the backend: whisper.cpp is
 * a native binary, so this view's job is to collect the settings, start the job,
 * and render progress from the same event channel the slice renders use.
 *
 * Two settings drive the output. Accuracy chooses the speech model *and* how
 * hard its decoder searches; words-per-caption decides how the resulting words
 * are grouped into subtitles.
 */

import { open, save } from "@tauri-apps/plugin-dialog";

import { api, toErrorPayload } from "./bridge";
import type { Actions, Chrome, Tool, ToolId } from "./tool";
import {
  MAX_WORDS_PER_CAPTION,
  MIN_WORDS_PER_CAPTION,
  type Accuracy,
  type ErrorPayload,
  type JobEvent,
  type TierStatus,
  type TranscriptSummary,
} from "./types";

interface State {
  inputPath: string;
  accuracy: Accuracy;
  wordsPerCaption: number;
  language: string;
  translate: boolean;
  outputDir: string;
  outputName: string;
  separator: string;
  jobId: string | null;
  summary: TranscriptSummary | null;
  tiers: TierStatus[];
  /** Starts optimistic so the tab does not flash a warning while tiers load. */
  engineAvailable: boolean;
  engineNote: string | null;
}

const DEFAULT_WORDS = 7;

/**
 * Extensions the file dialog offers.
 *
 * Video is included deliberately: FFmpeg strips the audio track before whisper
 * ever sees the file, so transcribing a video is the same operation.
 */
const AUDIO_EXTENSIONS = ["mp3", "wav", "m4a", "aac", "flac", "ogg", "opus", "wma"];
const VIDEO_EXTENSIONS = ["mp4", "mkv", "mov", "webm", "avi", "m4v"];

/** Whisper's better-supported languages, plus auto-detect. */
const LANGUAGES: ReadonlyArray<[string, string]> = [
  ["auto", "Detect automatically"],
  ["en", "English"],
  ["es", "Spanish"],
  ["fr", "French"],
  ["de", "German"],
  ["it", "Italian"],
  ["pt", "Portuguese"],
  ["nl", "Dutch"],
  ["ru", "Russian"],
  ["pl", "Polish"],
  ["tr", "Turkish"],
  ["ar", "Arabic"],
  ["hi", "Hindi"],
  ["ja", "Japanese"],
  ["ko", "Korean"],
  ["zh", "Chinese"],
];

function baseName(path: string): string {
  const cut = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return cut >= 0 ? path.slice(cut + 1) : path;
}

function srtNameFor(path: string): string {
  const stem = baseName(path).replace(/\.[^.]+$/, "") || "transcript";
  return `${stem}.srt`;
}

function formatClock(ms: number): string {
  const total = Math.round(ms / 1000);
  const m = Math.floor(total / 60);
  const s = total % 60;
  return `${m}:${String(s).padStart(2, "0")}`;
}

// ---------------------------------------------------------------- markup

const MARKUP = `
  <div class="alert error" data-el="alert" hidden>
    <strong data-el="alertMessage"></strong>
    <span class="alert-hint" data-el="alertHint"></span>
  </div>

  <section class="card">
    <h2 class="card-title">1 · Audio</h2>

    <div class="dropzone" data-el="dropzone" role="button" tabindex="0"
         aria-label="Drop an audio or video file here, or press Enter to browse">
      <svg class="dropzone-icon" viewBox="0 0 24 24" aria-hidden="true">
        <path d="M12 3v12.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/>
        <circle cx="9" cy="16.5" r="3" fill="none" stroke="currentColor" stroke-width="1.6"/>
        <path d="M12 3l6 2v3l-6-2" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/>
      </svg>
      <p class="dropzone-title">Drop audio or video here</p>
      <p class="dropzone-hint">or <span class="link">browse your files</span> — MP3, WAV, M4A, MP4…</p>
    </div>

    <div class="row file-line" data-el="fileLine" hidden>
      <span class="file-name" data-el="fileName"></span>
      <span class="grow"></span>
      <button class="chip" data-el="replaceBtn">Replace</button>
    </div>
  </section>

  <section class="card">
    <h2 class="card-title">2 · Transcription</h2>

    <div class="field-block">
      <span class="field-label">Accuracy</span>
      <div class="tier-grid" data-el="tierGrid" role="radiogroup" aria-label="Accuracy"></div>
      <p class="hint" data-el="tierNote"></p>
    </div>

    <div class="row wrap caption-row">
      <label class="field grow">
        <span>Words per caption — <b data-el="wordsValue">${DEFAULT_WORDS}</b></span>
        <input type="range" data-el="wordsRange"
               min="${MIN_WORDS_PER_CAPTION}" max="${MAX_WORDS_PER_CAPTION}"
               step="1" value="${DEFAULT_WORDS}" />
        <span class="hint" data-el="wordsHint"></span>
      </label>

      <label class="field">
        <span>Language</span>
        <select data-el="languageSelect">
          ${LANGUAGES.map(([code, name]) => `<option value="${code}">${name}</option>`).join("")}
        </select>
      </label>
    </div>

    <label class="check">
      <input type="checkbox" data-el="translate" />
      <span>Translate to English instead of transcribing verbatim</span>
    </label>
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
    <h2 class="card-title">3 · Result</h2>
    <div class="preview-bar">
      <span class="hint" data-el="resultMeta"></span>
      <span class="grow"></span>
      <button class="chip" data-el="revealBtn">Show in folder</button>
    </div>
    <div class="srt-preview" data-el="srtPreview"></div>
  </section>
`;

// ------------------------------------------------------------------ view

export class TranscribeView implements Tool {
  readonly id: ToolId = "transcribe";

  private readonly root: HTMLElement;
  private readonly chrome: Chrome;
  private readonly state: State;
  private readonly el: Record<string, HTMLElement> = {};
  private tiersLoaded = false;

  constructor(root: HTMLElement, chrome: Chrome) {
    this.root = root;
    this.chrome = chrome;

    this.state = {
      inputPath: "",
      accuracy: "normal",
      wordsPerCaption: DEFAULT_WORDS,
      language: "auto",
      translate: false,
      outputDir: "",
      outputName: "",
      separator: "/",
      jobId: null,
      summary: null,
      tiers: [],
      engineAvailable: true,
      engineNote: null,
    };

    root.classList.add("panel");
    root.setAttribute("role", "tabpanel");
    root.innerHTML = MARKUP;

    this.wire();
    this.syncWordsHint();
    this.syncActions();
  }

  private pick<T extends HTMLElement>(name: string): T {
    const cached = this.el[name];
    if (cached) return cached as T;

    const node = this.root.querySelector<T>(`[data-el="${name}"]`);
    if (!node) throw new Error(`transcribe: missing [data-el="${name}"]`);
    this.el[name] = node;
    return node;
  }

  // ------------------------------------------------------------ tab hooks

  activate(): void {
    this.root.hidden = false;
    void this.loadTiers();
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

  // --------------------------------------------------------------- wiring

  private wire(): void {
    const dropzone = this.pick("dropzone");

    dropzone.addEventListener("click", () => void this.browse());
    dropzone.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        void this.browse();
      }
    });
    this.pick("replaceBtn").addEventListener("click", () => void this.browse());

    // Tauri's own drag-drop handler is off, so these carry real File objects.
    // Only the path matters here: the backend reads the file itself, and a
    // browser File has no path, so the drop is used purely as a filename hint
    // when the dialog cannot be avoided.
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
      void this.acceptDrop(event.dataTransfer);
    });

    for (const type of ["dragover", "drop"] as const) {
      this.root.addEventListener(type, (event) => {
        if (!(event.target as HTMLElement).closest('[data-el="dropzone"]')) {
          event.preventDefault();
        }
      });
    }

    this.pick("tierGrid").addEventListener("click", (event) => {
      const card = (event.target as HTMLElement).closest<HTMLElement>("[data-tier]");
      const tier = card?.dataset.tier as Accuracy | undefined;
      if (tier && !card?.hasAttribute("data-unavailable")) {
        this.state.accuracy = tier;
        this.syncTiers();
        this.syncActions();
      }
    });

    const words = this.pick<HTMLInputElement>("wordsRange");
    words.addEventListener("input", () => {
      this.state.wordsPerCaption = Number(words.value);
      this.pick("wordsValue").textContent = String(this.state.wordsPerCaption);
      this.syncWordsHint();
    });

    this.pick<HTMLSelectElement>("languageSelect").addEventListener("change", (event) => {
      this.state.language = (event.target as HTMLSelectElement).value;
    });

    this.pick<HTMLInputElement>("translate").addEventListener("change", (event) => {
      this.state.translate = (event.target as HTMLInputElement).checked;
    });

    this.pick("cancelBtn").addEventListener("click", () => {
      if (!this.state.jobId) return;
      // whisper checks the token between segments, so a long file stops within
      // a segment rather than instantly. Say so instead of leaving a dead button.
      const button = this.pick<HTMLButtonElement>("cancelBtn");
      button.disabled = true;
      button.textContent = "Cancelling…";
      void api.cancelJob(this.state.jobId);
    });

    this.pick("revealBtn").addEventListener("click", () => {
      const path = this.state.summary?.outputPath;
      if (path) void api.revealInFolder(path);
    });
  }

  // --------------------------------------------------------------- inputs

  /**
   * A drop gives us a File, but the backend needs a filesystem path and the
   * webview refuses to expose one. So a drop opens the picker pre-aimed at the
   * dropped name rather than silently doing nothing.
   */
  private async acceptDrop(data: DataTransfer | null): Promise<void> {
    const dropped = data?.files?.[0];
    if (!dropped) return;
    await this.browse(dropped.name);
  }

  private async browse(suggested?: string): Promise<void> {
    if (this.busy) return;

    const chosen = await open({
      multiple: false,
      directory: false,
      title: suggested ? `Locate “${suggested}”` : "Choose audio or video",
      filters: [
        { name: "Audio and video", extensions: [...AUDIO_EXTENSIONS, ...VIDEO_EXTENSIONS] },
        { name: "Audio", extensions: AUDIO_EXTENSIONS },
        { name: "Video", extensions: VIDEO_EXTENSIONS },
      ],
    });
    if (typeof chosen !== "string") return;

    this.showError(null);
    this.state.inputPath = chosen;
    this.state.summary = null;
    this.pick("resultCard").hidden = true;

    this.pick("fileLine").hidden = false;
    this.pick("fileName").textContent = baseName(chosen);

    const cut = Math.max(chosen.lastIndexOf("/"), chosen.lastIndexOf("\\"));
    if (cut > 0) {
      this.state.separator = chosen[cut] ?? this.state.separator;
      // Default to writing the .srt beside the audio, which is where a player
      // looks for it.
      this.state.outputDir = chosen.slice(0, cut);
    } else {
      this.state.outputDir = this.chrome.defaults.dir;
      this.state.separator = this.chrome.defaults.separator;
    }
    this.state.outputName = srtNameFor(chosen);

    this.syncActions();
  }

  // ---------------------------------------------------------------- tiers

  private async loadTiers(): Promise<void> {
    if (this.tiersLoaded) return;

    try {
      const status = await api.transcriptionTiers();
      this.state.tiers = status.tiers;
      this.state.engineAvailable = status.engineAvailable;
      this.state.engineNote = status.engineNote;
    } catch (error) {
      this.showError(toErrorPayload(error));
      return;
    }
    this.tiersLoaded = true;

    // Prefer the requested default, but never preselect a tier whose model is
    // not installed — the Transcribe button would just fail.
    const available = this.state.tiers.filter((t) => t.available);
    if (!available.some((t) => t.id === this.state.accuracy)) {
      const fallback = available[0]?.id;
      if (fallback) this.state.accuracy = fallback;
    }

    this.syncTiers();
    this.syncActions();
  }

  private syncTiers(): void {
    const grid = this.pick("tierGrid");

    // A missing engine disables every tier for a reason that has nothing to do
    // with the model files, so it takes over the messaging wherever it applies.
    const engineNote = this.state.engineAvailable
      ? null
      : (this.state.engineNote ?? "Speech transcription is unavailable on this system.");

    grid.replaceChildren(
      ...this.state.tiers.map((tier) => {
        const card = document.createElement("button");
        card.type = "button";
        card.className = "tier";
        card.dataset.tier = tier.id;
        card.setAttribute("role", "radio");
        card.setAttribute("aria-checked", String(tier.id === this.state.accuracy));
        card.classList.toggle("active", tier.id === this.state.accuracy);

        if (!tier.available) {
          card.setAttribute("data-unavailable", "");
          card.disabled = true;
          card.title =
            engineNote ??
            (tier.installDir
              ? `${tier.model} is not installed. Put it in ${tier.installDir} and reopen this tab.`
              : `${tier.model} is not installed.`);
        }

        const name = document.createElement("span");
        name.className = "tier-name";
        name.textContent = tier.label;

        const detail = document.createElement("span");
        detail.className = "tier-detail";
        detail.textContent = tier.available
          ? `${tier.model.replace(/^ggml-|\.bin$/g, "")} · ${this.speedLabel(tier)}`
          : engineNote
            ? "no engine"
            : "not installed";

        card.append(name, detail);
        return card;
      }),
    );

    const missing = this.state.tiers.filter((t) => !t.available);
    const note = this.pick("tierNote");
    note.replaceChildren();

    if (engineNote) {
      note.textContent = engineNote;
      return;
    }

    if (!missing.length) {
      note.textContent =
        "Higher accuracy uses a larger model and searches more candidate transcriptions.";
      return;
    }

    const names = missing.map((t) => t.label).join(" and ");
    const dir = missing.find((t) => t.installDir)?.installDir;

    if (!dir) {
      note.textContent = `${names} unavailable — the model file is missing.`;
      return;
    }

    // Name the exact folder rather than describing it: this is a path the user
    // has to get right, and the model is too large to have been bundled.
    note.append(
      `${names} needs its model file — too large to ship inside the installer. Put `,
      Object.assign(document.createElement("code"), {
        textContent: missing.map((t) => t.model).join(" / "),
      }),
      " in ",
      Object.assign(document.createElement("code"), { textContent: dir }),
      " and reopen this tab. ",
    );

    const copy = document.createElement("button");
    copy.className = "link";
    copy.type = "button";
    copy.textContent = "Copy folder path";
    copy.addEventListener("click", () => {
      void navigator.clipboard.writeText(dir).then(
        () => {
          copy.textContent = "Copied";
          setTimeout(() => (copy.textContent = "Copy folder path"), 1500);
        },
        () => (copy.textContent = "Copy failed"),
      );
    });
    note.append(copy);
  }

  /** Turn the realtime factor into an estimate for the loaded file. */
  private speedLabel(tier: TierStatus): string {
    return tier.realtimeFactor >= 1
      ? `~${tier.realtimeFactor.toFixed(1)}× audio length`
      : `~${Math.round(1 / tier.realtimeFactor)}× realtime`;
  }

  private syncWordsHint(): void {
    const n = this.state.wordsPerCaption;
    const hint =
      n <= 2
        ? "Very short captions — one or two words at a time, karaoke style."
        : n <= 5
          ? "Short captions, easy to read on a phone."
          : n <= 10
            ? "A comfortable subtitle line."
            : "Long captions — may not fit on a narrow screen.";
    this.pick("wordsHint").textContent =
      `${hint} Captions also break at sentence ends and pauses, so some will be shorter.`;
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

  private currentOutputPath(): string {
    const { outputDir, outputName, separator } = this.state;
    if (!outputName) return "";
    return outputDir ? `${outputDir}${separator}${outputName}` : outputName;
  }

  // ------------------------------------------------------------ the job

  private setProgress(percent: number | null, stage: string, note = ""): void {
    const card = this.pick("progressCard");
    card.hidden = false;

    this.pick("progressStage").textContent = stage;
    this.pick("progressPct").textContent = percent === null ? "" : `${Math.round(percent)}%`;
    this.pick("progressNote").textContent = note;

    const bar = this.pick("progressBar");
    bar.classList.toggle("indeterminate", percent === null);
    const fill = bar.firstElementChild as HTMLElement | null;
    if (fill && percent !== null) fill.style.width = `${percent}%`;
  }

  handleProgress(event: JobEvent): void {
    if (event.jobId !== this.state.jobId) return;

    switch (event.stage) {
      case "done":
        this.state.jobId = null;
        this.pick("progressCard").hidden = true;
        this.syncActions();
        break;

      case "failed":
        this.state.jobId = null;
        this.pick("progressCard").hidden = true;
        this.showError(event.error ?? { kind: "unknown", message: event.message, hint: null });
        this.syncActions();
        break;

      // The job writes its .srt only after whisper finishes and clears its
      // scratch directory on every exit path, so a cancel really does leave
      // nothing behind. The card stays up to say that, as the slice tab does.
      case "cancelled":
        this.state.jobId = null;
        this.pick("progressBar").className = "bar failed";
        this.pick("cancelBtn").hidden = true;
        this.pick("progressNote").textContent = "Cancelled — nothing was written.";
        this.syncActions();
        break;

      default:
        this.setProgress(
          event.percent,
          event.message,
          "Everything runs on this machine — nothing is uploaded.",
        );
    }
  }

  /** Called by the shell when a transcript summary arrives. */
  acceptSummary(summary: TranscriptSummary): void {
    this.state.summary = summary;
    this.pick("resultCard").hidden = false;

    const spoken = summary.language ? summary.language.toUpperCase() : "unknown";
    this.pick("resultMeta").textContent =
      `${summary.captions} captions · ${summary.words} words · ${spoken} · ` +
      `${formatClock(summary.durationMs)} · ${Math.round(summary.confidence * 100)}% confidence`;

    const preview = this.pick("srtPreview");
    preview.replaceChildren(
      ...summary.preview.map((caption) => {
        const row = document.createElement("div");
        row.className = "srt-row";
        // A visibly shaky caption is worth flagging: it is the one the user
        // will want to check by ear.
        if (caption.confidence < 0.6) row.classList.add("shaky");

        const time = document.createElement("span");
        time.className = "srt-time";
        time.textContent = `${caption.start} → ${caption.end}`;

        const text = document.createElement("span");
        text.className = "srt-text";
        text.textContent = caption.text;

        row.append(time, text);
        return row;
      }),
    );

    if (summary.captions > summary.preview.length) {
      const more = document.createElement("p");
      more.className = "hint";
      more.textContent = `…and ${summary.captions - summary.preview.length} more, written to the file.`;
      preview.append(more);
    }

    this.syncActions();
  }

  // -------------------------------------------------------- the action bar

  private syncActions(): void {
    const tierReady = this.state.tiers.some(
      (t) => t.id === this.state.accuracy && t.available,
    );
    const ready = this.state.inputPath !== "" && tierReady;

    const actions: Actions = {
      canChoose: ready && !this.busy,
      canRender: ready && !this.busy && this.state.outputName !== "",
      busy: this.busy,
      path: this.state.summary
        ? `Saved to ${this.state.summary.outputPath}`
        : ready
          ? this.currentOutputPath()
          : "No audio loaded",
      chooseLabel: "Save to…",
      renderLabel: "Transcribe",
      busyLabel: "Transcribing…",
    };

    this.chrome.setActions(this, actions);
  }

  async chooseDestination(): Promise<void> {
    const chosen = await save({
      defaultPath: this.currentOutputPath(),
      filters: [{ name: "SubRip subtitles", extensions: ["srt"] }],
    });
    if (!chosen) return;

    const cut = Math.max(chosen.lastIndexOf("/"), chosen.lastIndexOf("\\"));
    if (cut > 0) {
      this.state.separator = chosen[cut] ?? this.state.separator;
      this.state.outputDir = chosen.slice(0, cut);
      this.state.outputName = chosen.slice(cut + 1);
    } else {
      this.state.outputName = chosen;
    }

    this.state.summary = null;
    this.syncActions();
  }

  async startRender(): Promise<void> {
    if (this.busy || !this.state.inputPath) return;

    this.showError(null);
    this.state.summary = null;
    this.pick("resultCard").hidden = true;

    // A previous cancel leaves the card in its stopped state. Clear it before
    // setProgress runs, which is what puts the bar back into indeterminate.
    this.pick("progressBar").className = "bar";
    const cancelBtn = this.pick<HTMLButtonElement>("cancelBtn");
    cancelBtn.hidden = false;
    cancelBtn.disabled = false;
    cancelBtn.textContent = "Cancel";

    this.setProgress(null, "Starting…");

    try {
      this.state.jobId = await api.startTranscription({
        inputPath: this.state.inputPath,
        accuracy: this.state.accuracy,
        wordsPerCaption: this.state.wordsPerCaption,
        language: this.state.language,
        translate: this.state.translate,
        outputPath: this.currentOutputPath(),
      });
    } catch (error) {
      this.pick("progressCard").hidden = true;
      this.showError(toErrorPayload(error));
    }

    this.syncActions();
  }
}
