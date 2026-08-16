/**
 * Vendors everything the Remove Background tool needs to run offline:
 * the segmentation models, and the ONNX Runtime WASM binary that executes them.
 *
 *   node scripts/fetch-model.mjs                  # both tiers — the default
 *   node scripts/fetch-model.mjs --model isnet    # precise only
 *   node scripts/fetch-model.mjs --model u2netp   # fast only (small download)
 *   node scripts/fetch-model.mjs --list
 *
 * Everything lands in `public/`, which Vite serves at the site root in dev and
 * copies verbatim into `dist/` on build — so the shipped app resolves it from
 * its own origin. It is gitignored: build inputs, not source, exactly like the
 * sidecar binaries in `src-tauri/binaries/`.
 *
 * This script is the ONLY part of the background remover that touches the
 * network, and it runs at setup time. Once `public/` is populated the tool never
 * makes a request again — see `src/matte.ts`, which pins the runtime's asset
 * paths to these local files.
 */

import { createHash } from "node:crypto";
import { copyFileSync, existsSync, mkdirSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const MODEL_DIR = join(ROOT, "public", "models");
const ORT_DIR = join(ROOT, "public", "ort");

/**
 * Pinned so a vendored build is reproducible; bump deliberately.
 *
 * Both are published by the rembg project under a single evergreen release tag.
 * `bytes` and `sha256` are verified after download — a truncated or substituted
 * model would otherwise surface as a baffling inference error at runtime.
 *
 * Preprocessing travels with each model rather than being assumed shared. These
 * two agree on almost nothing: IS-Net takes a 1024² input centred on 0.5 with
 * unit scale, while U²-Net takes 320² with ImageNet statistics. Each rejects the
 * other's input shape outright, so a single shared table would produce a
 * dimension error rather than a merely worse cut-out.
 *
 * The quality gap is a resolution gap. Measured on a 1200×900 scene with 3 px
 * strands, u2netp recovers them at alpha 33/255 while isnet holds them at 255 —
 * a 320² mask carries one mask pixel per ~4 image pixels, so hair, fur, and
 * foliage smear away no matter how the alpha is post-processed.
 */
const REMBG = "https://github.com/danielgatis/rembg/releases/download/v0.0.0";

const MODELS = {
  isnet: {
    tier: "precise",
    label: "Precise",
    detail: "~10 s · resolves hair and fine detail",
    file: "isnet-general-use.onnx",
    url: `${REMBG}/isnet-general-use.onnx`,
    bytes: 178648008,
    sha256: "60920e99c45464f2ba57bee2ad08c919a52bbf852739e96947fbb4358c0d964a",
    size: 1024,
    mean: [0.5, 0.5, 0.5],
    std: [1.0, 1.0, 1.0],
    note: "IS-Net general · 170 MB · 1024² mask · ~10 s · much sharper edges",
  },
  u2netp: {
    tier: "fast",
    label: "Fast",
    detail: "~1.4 s · softens thin detail",
    file: "u2netp.onnx",
    url: `${REMBG}/u2netp.onnx`,
    bytes: 4574861,
    sha256: "309c8469258dda742793dce0ebea8e6dd393174f89934733ecc8b14c76f4ddd8",
    size: 320,
    mean: [0.485, 0.456, 0.406],
    std: [0.229, 0.224, 0.225],
    note: "U²-Net lite · 4.4 MB · 320² mask · ~1.4 s · soft edges",
  },
};

/** Which tier the tool reaches for when the user has not chosen. */
const DEFAULT_TIER = "precise";

/**
 * The runtime itself: the WebAssembly module and the Emscripten glue that
 * instantiates it.
 *
 * Both are needed. Despite the name, the `ort.wasm.bundle.min.mjs` entry point
 * `src/matte.ts` imports does *not* inline the glue — it differs from the plain
 * entry by one string — so the `.mjs` is fetched at runtime like the `.wasm` is.
 * Vendoring only the `.wasm` leaves a 404 that surfaces on first use.
 *
 * The non-JSEP build is deliberate: JSEP exists to bridge WebGPU, which the tool
 * does not use, and costs an extra 10 MB to ship.
 */
const ORT_FILES = ["ort-wasm-simd-threaded.wasm", "ort-wasm-simd-threaded.mjs"];
const ORT_DIST = join(ROOT, "node_modules", "onnxruntime-web", "dist");

// --------------------------------------------------------------- utilities

function mb(bytes) {
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

async function download(url, destination, expected) {
  process.stdout.write(`  ↓ ${url.split("/").pop()} (${mb(expected.bytes)})\n`);

  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) {
    throw new Error(`HTTP ${response.status} ${response.statusText} for ${url}`);
  }

  const bytes = Buffer.from(await response.arrayBuffer());

  if (expected.bytes && bytes.length !== expected.bytes) {
    throw new Error(
      `${destination}: expected ${expected.bytes} bytes, got ${bytes.length}. ` +
        `The upstream asset changed — verify it, then update the pin in this script.`,
    );
  }

  const digest = createHash("sha256").update(bytes).digest("hex");
  if (expected.sha256 && digest !== expected.sha256) {
    throw new Error(
      `${destination}: sha256 mismatch.\n  expected ${expected.sha256}\n  got      ${digest}`,
    );
  }

  writeFileSync(destination, bytes);
  return { size: bytes.length, digest };
}

// -------------------------------------------------------------------- main

async function main() {
  const argv = process.argv.slice(2);

  if (argv.includes("--list")) {
    console.log("Available models:\n");
    for (const [id, model] of Object.entries(MODELS)) {
      console.log(`  ${id.padEnd(8)} ${model.tier.padEnd(8)} ${model.note}`);
    }
    console.log("\nBoth are vendored by default, and selectable in the app.");
    return;
  }

  const requested = argv.includes("--model")
    ? [argv[argv.indexOf("--model") + 1]]
    : Object.keys(MODELS);

  for (const id of requested) {
    if (!MODELS[id]) {
      throw new Error(`Unknown model "${id}". Run with --list to see the options.`);
    }
  }

  mkdirSync(MODEL_DIR, { recursive: true });
  mkdirSync(ORT_DIR, { recursive: true });

  // --- models ------------------------------------------------------------

  for (const id of requested) {
    const model = MODELS[id];
    const path = join(MODEL_DIR, model.file);

    if (existsSync(path) && statSync(path).size === model.bytes) {
      console.log(`  = ${model.file} already vendored (${mb(model.bytes)})`);
      continue;
    }

    console.log(`Fetching ${id} — ${model.note}`);
    const { size } = await download(model.url, path, model);
    console.log(`  ✓ ${model.file} (${mb(size)})`);
  }

  // --- runtime -----------------------------------------------------------

  for (const name of ORT_FILES) {
    const source = join(ORT_DIST, name);
    if (!existsSync(source)) {
      throw new Error(
        `${source} is missing. Run \`npm install\` first — the runtime ships inside ` +
          `the onnxruntime-web package rather than being downloaded separately.`,
      );
    }

    const dest = join(ORT_DIR, name);
    copyFileSync(source, dest);
    console.log(`  ✓ ${name} (${mb(statSync(dest).size)})`);
  }

  // --- manifest ----------------------------------------------------------

  // `matte.ts` reads this to learn which tiers exist and how to feed each one,
  // so vendoring a different set is a re-run of this script rather than a code
  // edit. Only tiers whose weights are actually present are listed, which is
  // what lets the UI hide a toggle that would have nothing to switch to.
  const tiers = {};
  for (const [id, model] of Object.entries(MODELS)) {
    if (!existsSync(join(MODEL_DIR, model.file))) continue;
    tiers[model.tier] = {
      id,
      label: model.label,
      detail: model.detail,
      file: model.file,
      size: model.size,
      mean: model.mean,
      std: model.std,
    };
  }

  if (Object.keys(tiers).length === 0) {
    throw new Error("No models were vendored — nothing for the tool to run.");
  }

  const manifest = {
    // Fall back to whichever tier exists if the preferred one was not vendored.
    default: tiers[DEFAULT_TIER] ? DEFAULT_TIER : Object.keys(tiers)[0],
    tiers,
    wasm: ORT_FILES[0],
    mjs: ORT_FILES[1],
  };

  writeFileSync(join(MODEL_DIR, "manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
  console.log(`  ✓ manifest.json (${Object.keys(tiers).join(", ")}; default ${manifest.default})`);

  console.log("\nRemove Background is ready — assets resolve locally, no network at runtime.");
}

main().catch((error) => {
  console.error(`\n✗ ${error.message}`);
  process.exit(1);
});
