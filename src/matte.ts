/**
 * Local image matting — the engine behind the Remove Background tab.
 *
 * Everything here runs inside the webview on the user's machine: ONNX Runtime
 * compiled to WebAssembly, executing a U²-Net saliency model over the pixels of
 * a file the user picked. Nothing is uploaded, and nothing is fetched.
 *
 * ## The no-network guarantee
 *
 * ONNX Runtime needs two files it does not bundle — the WebAssembly module and
 * the Emscripten glue that instantiates it — and by default it resolves them
 * against its own script URL. Under Vite's hashed-asset layout that resolves to
 * nothing, and a runtime left to guess is a runtime that might one day guess a
 * CDN. So nothing is left to it:
 *
 *   1. The `.wasm` is handed over as an {@link ArrayBuffer} via
 *      `env.wasm.wasmBinary`. When it is set, no URL is constructed for it.
 *   2. The `.mjs` is pinned to an absolute same-origin URL via `env.wasm.
 *      wasmPaths`, rather than a prefix the runtime completes itself.
 *   3. `numThreads = 1` and `proxy = false` stop it spawning workers, which
 *      would need their own script URL (and a cross-origin isolation we do not
 *      have under Tauri's custom protocol).
 *
 * Both assets live in `public/`, vendored by `npm run model`, and are resolved
 * against `document.baseURI` — the dev server in development, the app's own
 * bundle in a release build.
 *
 * The backstop is the CSP in `tauri.conf.json`: `connect-src` is limited to
 * `'self'` and the IPC endpoints, so a fetch to anywhere else fails regardless
 * of what any dependency decides to do.
 *
 * ## Laziness
 *
 * Nothing in this module is touched until the user opens the tab and drops an
 * image: the runtime import is dynamic, and the session is built on first use
 * and then cached for the life of the window. App launch is unaffected — the
 * ~15 MB of runtime and weights is never read on a run where the tool is unused.
 */

import type { InferenceSession, Tensor } from "onnxruntime-web";

/** Which model a cut-out should use. */
export type Tier = "fast" | "precise";

/** One vendored model and everything needed to feed it. */
export interface TierSpec {
  /** Upstream model name, e.g. `isnet`. */
  id: string;
  /** UI label, e.g. `Precise`. */
  label: string;
  /** UI sublabel: rough cost and what it buys. */
  detail: string;
  file: string;
  /** Square input edge the network was traced at. */
  size: number;
  mean: [number, number, number];
  std: [number, number, number];
}

/** What was vendored. Written by `scripts/fetch-model.mjs`. */
interface Manifest {
  default: Tier;
  tiers: Partial<Record<Tier, TierSpec>>;
  /** Filenames under `public/ort/`. */
  wasm: string;
  mjs: string;
}

export type Phase = "runtime" | "decoding" | "segmenting" | "compositing";

export interface Progress {
  phase: Phase;
  message: string;
  /** 0–100, or null when the step has no measurable extent. */
  percent: number | null;
}

export interface Matte {
  /** The cut-out, as straight (non-premultiplied) RGBA. */
  canvas: HTMLCanvasElement;
  width: number;
  height: number;
  /** Milliseconds spent inside `session.run`. */
  inferenceMs: number;
  /** Set when the source exceeded {@link MAX_EDGE} and was scaled to fit. */
  scaledFrom: { width: number; height: number } | null;
  /** Which tier produced this, and the mask resolution it worked at. */
  tier: Tier;
  maskSize: number;
}

/**
 * Longest edge we will composite at.
 *
 * Canvas backing stores are capped by the browser (WebView2 refuses past
 * ~16k, and allocates eagerly long before that), and the mask itself carries
 * only 320 px of real detail, so upsampling a 12000 px original buys nothing but
 * a 500 MB allocation. Anything larger is scaled down and the user is told.
 */
const MAX_EDGE = 4096;

/**
 * Alpha below this is treated as numerical noise rather than signal.
 *
 * Deliberately tiny. An earlier version clamped at 0.08 and rescaled what was
 * left, on the theory that it removed the faint wash U²-Net leaves over empty
 * space. It did — along with every strand of hair, every soft shadow edge, and
 * every motion-blurred boundary, because wispy detail genuinely lives at 5–20%
 * alpha. Suppressing the tail is exactly the wrong trade for a matting tool:
 * haze is a cosmetic complaint, erased hair is a broken cut-out.
 *
 * What remains kills only values that would round to zero in 8-bit anyway.
 */
const ALPHA_NOISE_FLOOR = 1 / 512;

/** Formats we hand to `createImageBitmap`. */
const ACCEPTED = new Set(["image/png", "image/jpeg", "image/webp", "image/bmp", "image/gif"]);
const ACCEPTED_EXTENSIONS = /\.(png|jpe?g|webp|bmp|gif)$/i;

/** A failure the UI can show verbatim, with a next step where one exists. */
/**
 * Thrown by {@link cutout} when the caller's `shouldStop` asks it to end.
 *
 * A distinct type rather than a `MatteError` because it is not a failure: the
 * tab discards it silently instead of showing the user an error they caused.
 */
export class MatteCancelled extends Error {
  constructor() {
    super("Cancelled");
    this.name = "MatteCancelled";
  }
}

export class MatteError extends Error {
  readonly hint: string | null;

  constructor(message: string, hint: string | null = null) {
    super(message);
    this.name = "MatteError";
    this.hint = hint;
  }
}

// ------------------------------------------------------------------ assets

function assetUrl(path: string): string {
  return new URL(path, document.baseURI).href;
}

async function readLocal(path: string, what: string): Promise<Response> {
  let response: Response;
  try {
    response = await fetch(assetUrl(path));
  } catch {
    throw new MatteError(
      `Could not read the ${what} from the app bundle.`,
      "Run `npm run model` to vendor it into public/, then restart the app.",
    );
  }

  // A dev server answers a missing file with index.html rather than a 404, so a
  // 200 alone is not proof; the caller checks the payload size too.
  if (!response.ok) {
    throw new MatteError(
      `The ${what} is missing from the app bundle (HTTP ${response.status}).`,
      "Run `npm run model` to vendor it into public/, then restart the app.",
    );
  }
  return response;
}

// ----------------------------------------------------------------- session

type Runtime = {
  ort: typeof import("onnxruntime-web");
  manifest: Manifest;
};

/**
 * The manifest alone — a few hundred bytes naming what was vendored.
 *
 * Deliberately separate from {@link runtime}: the UI needs to know which tiers
 * exist in order to draw the quality toggle, and that question must not drag in
 * a dynamic import and 10 MB of WebAssembly before the user has asked for a
 * cut-out.
 */
let manifestPending: Promise<Manifest> | null = null;

function loadManifest(): Promise<Manifest> {
  if (!manifestPending) {
    manifestPending = (async () => {
      const response = await readLocal("models/manifest.json", "model manifest");
      let manifest: Manifest;
      try {
        manifest = (await response.json()) as Manifest;
      } catch {
        throw new MatteError(
          "The model manifest in public/models is not readable.",
          "Run `npm run model` to re-vendor it.",
        );
      }

      if (!manifest.tiers || Object.keys(manifest.tiers).length === 0) {
        throw new MatteError(
          "No segmentation model is vendored in public/models.",
          "Run `npm run model`, then restart the app.",
        );
      }
      return manifest;
    })().catch((error) => {
      manifestPending = null;
      throw error;
    });
  }
  return manifestPending;
}

/**
 * The configured runtime, loaded once for the window.
 *
 * Held as the promise rather than the resolved value so that two images dropped
 * in quick succession share one load instead of racing to configure ONNX Runtime
 * twice.
 */
let runtimePending: Promise<Runtime> | null = null;

async function bootRuntime(): Promise<Runtime> {
  const manifest = await loadManifest();

  // The runtime is a dynamic import so it stays out of the startup chunk.
  const ort = await import("onnxruntime-web/wasm");

  const wasm = await (await readLocal(`ort/${manifest.wasm}`, "ONNX Runtime binary")).arrayBuffer();
  if (wasm.byteLength < 1_000_000) {
    throw new MatteError(
      "The ONNX Runtime binary in public/ort looks truncated.",
      "Run `npm run model` to re-vendor it.",
    );
  }

  // The runtime imports the glue itself, so a missing one would surface as an
  // opaque module error from inside the library. Fail here instead, where the
  // message can name the file and the command that puts it back.
  await readLocal(`ort/${manifest.mjs}`, "ONNX Runtime loader");

  ort.env.wasm.wasmBinary = wasm;
  // Absolute per-file URLs rather than a prefix: a prefix still leaves the
  // runtime to append a filename of its choosing, and the JSEP and non-JSEP
  // builds do not agree on what that filename is.
  ort.env.wasm.wasmPaths = {
    wasm: assetUrl(`ort/${manifest.wasm}`),
    mjs: assetUrl(`ort/${manifest.mjs}`),
  };
  ort.env.wasm.numThreads = 1;
  ort.env.logLevel = "error";

  // Run inference on a worker rather than the window's own thread.
  //
  // WebAssembly inference is a single synchronous call: at 1024² it holds the
  // main thread for ~11 seconds, during which nothing repaints — the progress
  // bar sits frozen and Windows ghosts the window as "not responding". Proxying
  // moves that call to a worker and leaves the UI live.
  //
  // This is not the same thing as WASM threading, which would need
  // SharedArrayBuffer and therefore cross-origin isolation. Enabling that would
  // mean COOP/COEP headers on every response, which would break the YouTube
  // embed the preview tab depends on. The proxy talks over postMessage, costs
  // nothing at the CSP level beyond a same-origin worker, and leaves
  // `numThreads` at 1.
  ort.env.wasm.proxy = true;

  return { ort, manifest };
}

function runtime(): Promise<Runtime> {
  if (!runtimePending) {
    runtimePending = bootRuntime().catch((error) => {
      // A failed load must not poison the cache — the user may fix `public/`
      // and try again without restarting.
      runtimePending = null;
      throw error;
    });
  }
  return runtimePending;
}

/**
 * One built session per tier.
 *
 * Both are kept once loaded. Toggling quality is a deliberate act the user is
 * watching, and rebuilding a session costs a few hundred milliseconds each way;
 * holding the weights instead trades resident memory — which on a desktop we
 * have — for a toggle that responds immediately.
 */
const sessions = new Map<Tier, Promise<InferenceSession>>();

async function sessionFor(
  tier: Tier,
  report: (progress: Progress) => void,
): Promise<{ ort: Runtime["ort"]; session: InferenceSession; spec: TierSpec }> {
  const { ort, manifest } = await runtime();

  const spec = manifest.tiers[tier];
  if (!spec) {
    throw new MatteError(
      `The “${tier}” quality model is not vendored in this build.`,
      "Run `npm run model` to vendor both, then restart the app.",
    );
  }

  let built = sessions.get(tier);
  if (!built) {
    report({
      phase: "runtime",
      message: `Loading the ${spec.label.toLowerCase()} model…`,
      percent: null,
    });

    built = (async () => {
      const weights = await (
        await readLocal(`models/${spec.file}`, "segmentation model")
      ).arrayBuffer();

      if (weights.byteLength < 100_000) {
        throw new MatteError(
          `The model file public/models/${spec.file} looks truncated.`,
          "Run `npm run model` to re-vendor it.",
        );
      }

      try {
        return await ort.InferenceSession.create(weights, {
          executionProviders: ["wasm"],
          graphOptimizationLevel: "all",
        });
      } catch (cause) {
        throw new MatteError(
          `The segmentation model could not be loaded: ${(cause as Error).message}`,
          "Run `npm run model` to re-vendor it, then restart the app.",
        );
      }
    })();

    sessions.set(
      tier,
      built.catch((error) => {
        sessions.delete(tier);
        throw error;
      }),
    );
  }

  return { ort, session: await built, spec };
}

/** Whether this tier's weights are already resident. */
export function isLoaded(tier: Tier): boolean {
  return sessions.has(tier);
}

/**
 * The vendored tiers, and which one to use by default.
 *
 * Reads only the manifest, so the UI can draw its toggle without paying for the
 * runtime.
 */
export async function tiers(): Promise<{ default: Tier; available: Array<[Tier, TierSpec]> }> {
  const manifest = await loadManifest();
  return {
    default: manifest.default,
    available: Object.entries(manifest.tiers) as Array<[Tier, TierSpec]>,
  };
}

// ------------------------------------------------------------------ decode

/** Reject obviously wrong files before spending a decode on them. */
export function looksLikeImage(file: File): boolean {
  if (file.type) return ACCEPTED.has(file.type.toLowerCase());
  // Some sources (and Windows drag sources in particular) hand over an empty
  // MIME type, so fall back to the extension rather than refusing outright.
  return ACCEPTED_EXTENSIONS.test(file.name);
}

export async function decode(file: File): Promise<ImageBitmap> {
  if (!looksLikeImage(file)) {
    throw new MatteError(
      `“${file.name}” is not an image this tool can read.`,
      "Supported formats: PNG, JPEG, WEBP, BMP, and GIF.",
    );
  }
  if (file.size === 0) {
    throw new MatteError(`“${file.name}” is empty.`);
  }

  try {
    return await createImageBitmap(file);
  } catch {
    throw new MatteError(
      `“${file.name}” could not be decoded — the file may be corrupt or mislabelled.`,
      "Supported formats: PNG, JPEG, WEBP, BMP, and GIF.",
    );
  }
}

// ------------------------------------------------------------------ canvas

function canvasOf(width: number, height: number): [HTMLCanvasElement, CanvasRenderingContext2D] {
  const canvas = document.createElement("canvas");
  canvas.width = width;
  canvas.height = height;

  // `willReadFrequently` matters here: every one of these canvases exists to be
  // read back with getImageData, and without the hint Chromium keeps them on the
  // GPU and pays a readback stall per call.
  const context = canvas.getContext("2d", { willReadFrequently: true });
  if (!context) {
    throw new MatteError(
      "This system's webview refused to provide a 2D canvas, so images cannot be processed.",
    );
  }
  return [canvas, context];
}

/** Longest-edge-bounded dimensions, preserving aspect ratio. */
function fit(width: number, height: number, max: number) {
  const longest = Math.max(width, height);
  if (longest <= max) return { width, height, scaled: false };

  const ratio = max / longest;
  return {
    width: Math.max(1, Math.round(width * ratio)),
    height: Math.max(1, Math.round(height * ratio)),
    scaled: true,
  };
}

// -------------------------------------------------------------- inference

/**
 * Square-resize into the tensor layout the network expects: NCHW float32,
 * per-channel ImageNet normalisation, planar rather than interleaved.
 */
function toTensor(
  ort: typeof import("onnxruntime-web"),
  bitmap: ImageBitmap,
  spec: TierSpec,
): Tensor {
  const side = spec.size;
  const [, context] = canvasOf(side, side);

  context.imageSmoothingEnabled = true;
  context.imageSmoothingQuality = "high";
  // Aspect ratio is deliberately not preserved: the model was trained on
  // squashed squares, and letterboxing instead teaches it that the bars are
  // background it should keep looking at.
  context.drawImage(bitmap, 0, 0, side, side);

  const { data } = context.getImageData(0, 0, side, side);
  const pixels = side * side;
  const input = new Float32Array(pixels * 3);

  const [mr, mg, mb] = spec.mean;
  const [sr, sg, sb] = spec.std;

  for (let i = 0; i < pixels; i += 1) {
    const p = i * 4;
    input[i] = ((data[p]! / 255) - mr) / sr;
    input[pixels + i] = ((data[p + 1]! / 255) - mg) / sg;
    input[pixels * 2 + i] = ((data[p + 2]! / 255) - mb) / sb;
  }

  return new ort.Tensor("float32", input, [1, 3, side, side]);
}

/**
 * Rescale the raw saliency map to 0–1 and clamp its tails.
 *
 * U²-Net's output is unbounded-ish per image, so the min/max are recomputed
 * every time rather than assumed — a low-contrast subject would otherwise come
 * out uniformly grey and matte to a translucent smear.
 */
function normalize(raw: Float32Array): Float32Array {
  let min = Infinity;
  let max = -Infinity;
  for (const value of raw) {
    if (value < min) min = value;
    if (value > max) max = value;
  }

  const mask = new Float32Array(raw.length);
  const spread = max - min;
  if (spread <= 1e-6) return mask; // Flat output: nothing salient was found.

  for (let i = 0; i < raw.length; i += 1) {
    const scaled = (raw[i]! - min) / spread;
    mask[i] = scaled < ALPHA_NOISE_FLOOR ? 0 : scaled;
  }
  return mask;
}

/**
 * Blow the square mask back up to the image's dimensions.
 *
 * The mask travels in the colour channels rather than in alpha, because canvas
 * stores alpha premultiplied — scaling it there would round every soft edge
 * through an 8-bit multiply and back, and fringe the result.
 */
function upscale(mask: Float32Array, side: number, width: number, height: number): Uint8ClampedArray {
  const [small, smallContext] = canvasOf(side, side);
  const grey = smallContext.createImageData(side, side);

  for (let i = 0; i < mask.length; i += 1) {
    const value = Math.round(mask[i]! * 255);
    const p = i * 4;
    grey.data[p] = value;
    grey.data[p + 1] = value;
    grey.data[p + 2] = value;
    grey.data[p + 3] = 255;
  }
  smallContext.putImageData(grey, 0, 0);

  const [, context] = canvasOf(width, height);
  context.imageSmoothingEnabled = true;
  context.imageSmoothingQuality = "high";
  context.drawImage(small, 0, 0, width, height);

  return context.getImageData(0, 0, width, height).data;
}

// --------------------------------------------------------------- pipeline

/**
 * Cut the background out of `bitmap`.
 *
 * The returned canvas is a fresh allocation the caller owns; `bitmap` is left
 * untouched so the UI can keep showing it as the "before" image.
 */
export async function cutout(
  bitmap: ImageBitmap,
  tier: Tier,
  onProgress: (progress: Progress) => void = () => {},
  shouldStop: () => boolean = () => false,
): Promise<Matte> {
  /**
   * Bail at a phase boundary if the caller has lost interest.
   *
   * Only boundaries: `session.run` below is a single WASM call that holds the
   * thread until it returns, and nothing short of moving inference into a
   * worker can interrupt it. Checking either side of it is what keeps a cancel
   * during model loading from paying for an inference nobody will look at.
   */
  const stopIfAsked = (): void => {
    if (shouldStop()) throw new MatteCancelled();
  };

  const { ort, session, spec } = await sessionFor(tier, onProgress);
  stopIfAsked();

  onProgress({ phase: "decoding", message: "Preparing the image…", percent: 15 });

  const target = fit(bitmap.width, bitmap.height, MAX_EDGE);
  const input = toTensor(ort, bitmap, spec);

  onProgress({ phase: "segmenting", message: "Finding the subject…", percent: 35 });

  // Yield once so the progress bar actually paints before the WASM call takes
  // the main thread for a second or two.
  await new Promise((resolve) => requestAnimationFrame(resolve));
  stopIfAsked();

  const started = performance.now();
  let outputs: InferenceSession.OnnxValueMapType;
  try {
    outputs = await session.run({ [session.inputNames[0]!]: input });
  } catch (cause) {
    throw new MatteError(
      `Segmentation failed: ${(cause as Error).message}`,
      "Try a smaller image, or re-vendor the model with `npm run model`.",
    );
  }
  const inferenceMs = performance.now() - started;
  stopIfAsked();

  // U²-Net exposes seven side outputs; the first is the fused, full-resolution
  // one and the rest are coarser supervision maps used only during training.
  const first = outputs[session.outputNames[0]!];
  if (!first) throw new MatteError("The model returned no output.");

  onProgress({ phase: "compositing", message: "Cutting out the subject…", percent: 80 });

  const mask = normalize(first.data as Float32Array);
  const alpha = upscale(mask, spec.size, target.width, target.height);

  const [canvas, context] = canvasOf(target.width, target.height);
  context.imageSmoothingEnabled = true;
  context.imageSmoothingQuality = "high";
  context.drawImage(bitmap, 0, 0, target.width, target.height);

  const image = context.getImageData(0, 0, target.width, target.height);
  for (let p = 3; p < image.data.length; p += 4) {
    // The mask is greyscale, so any colour channel carries it; read red.
    image.data[p] = alpha[p - 3]!;
  }
  context.putImageData(image, 0, 0);

  onProgress({ phase: "compositing", message: "Done.", percent: 100 });

  return {
    canvas,
    width: target.width,
    height: target.height,
    inferenceMs,
    scaledFrom: target.scaled ? { width: bitmap.width, height: bitmap.height } : null,
    tier,
    maskSize: spec.size,
  };
}

/** Encode a finished matte as PNG bytes, ready to write to disk. */
export async function toPng(canvas: HTMLCanvasElement): Promise<Uint8Array> {
  const blob = await new Promise<Blob | null>((resolve) => {
    canvas.toBlob(resolve, "image/png");
  });
  if (!blob) throw new MatteError("The cut-out could not be encoded as a PNG.");
  return new Uint8Array(await blob.arrayBuffer());
}
