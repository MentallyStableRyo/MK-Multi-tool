/**
 * Vendors the offline speech-to-text stack for the Audio → SRT tab:
 * the whisper.cpp binary and the ggml models it reads.
 *
 *   node scripts/fetch-whisper.mjs                  # binary + all three tiers
 *   node scripts/fetch-whisper.mjs --tier fast      # just one tier
 *   node scripts/fetch-whisper.mjs --tier fast,normal
 *   node scripts/fetch-whisper.mjs --binary-only
 *   node scripts/fetch-whisper.mjs --list
 *
 * Everything lands in `src-tauri/binaries/whisper/`, which is gitignored, and is
 * shipped as a bundle resource rather than through Tauri's `externalBin`.
 * `externalBin` renames single files and drops them next to the executable;
 * `whisper-cli` needs `whisper.dll` and a set of `ggml-cpu-*` backends beside
 * it, so it has to travel as a directory. `binaries.rs` knows that layout.
 *
 * The models are the whole download. All three together are about 2.1 GB, which
 * is why the tier list is selectable — a build that only ever needs Fast has no
 * reason to carry the large one.
 */

import {
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { inflateRawSync } from "node:zlib";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const DEST = join(ROOT, "src-tauri", "binaries", "whisper");
const MODEL_DEST = join(DEST, "models");

/** Pinned so a vendored build is reproducible; bump deliberately. */
const WHISPER_TAG = "v1.9.2";
const WHISPER_RELEASE = `https://github.com/ggml-org/whisper.cpp/releases/download/${WHISPER_TAG}`;
const HF = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

/**
 * Prebuilt binaries, per host.
 *
 * Only the plain CPU build is vendored. The BLAS variant is ~25% faster on the
 * encoder but drags in a 51 MB `libopenblas.dll`, and whisper.cpp already
 * dispatches to an AVX2/AVX-512 backend at runtime, so the trade is poor.
 * whisper.cpp publishes no macOS binary; that host falls back to a `whisper-cli`
 * on PATH (`brew install whisper-cpp`), which `binaries.rs` also resolves.
 */
const BINARIES = {
  "win32-x64": {
    archive: `${WHISPER_RELEASE}/whisper-bin-x64.zip`,
    /** Extracted from the archive; everything else in it is unrelated tooling. */
    keep: [
      "whisper-cli.exe",
      "whisper.dll",
      "ggml.dll",
      "ggml-base.dll",
      "ggml-cpu-alderlake.dll",
      "ggml-cpu-cannonlake.dll",
      "ggml-cpu-cascadelake.dll",
      "ggml-cpu-haswell.dll",
      "ggml-cpu-icelake.dll",
      "ggml-cpu-sandybridge.dll",
      "ggml-cpu-skylakex.dll",
      "ggml-cpu-sse42.dll",
      "ggml-cpu-x64.dll",
    ],
  },
};

/**
 * The three accuracy tiers, and the model each one runs.
 *
 * Multilingual builds throughout rather than the `.en` variants: whisper
 * auto-detects the language, and an English-only model would silently
 * mistranscribe everything else.
 */
const TIERS = {
  fast: {
    file: "ggml-base.bin",
    bytes: 147951465,
    note: "base · 141 MB · roughly 7× realtime",
  },
  normal: {
    file: "ggml-small.bin",
    bytes: 487601967,
    note: "small · 465 MB · roughly 2× realtime",
  },
  accurate: {
    file: "ggml-large-v3-turbo.bin",
    bytes: 1624555275,
    note: "large-v3-turbo · 1.5 GB · slower than realtime",
  },
};

// --------------------------------------------------------------- utilities

function mb(bytes) {
  return bytes >= 1024 * 1024 * 1024
    ? `${(bytes / 1024 ** 3).toFixed(2)} GB`
    : `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

async function download(url, destination, expectedBytes) {
  process.stdout.write(`  ↓ ${url.split("/").pop()}\n`);

  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) {
    throw new Error(`HTTP ${response.status} ${response.statusText} for ${url}`);
  }

  const bytes = Buffer.from(await response.arrayBuffer());
  if (expectedBytes && bytes.length !== expectedBytes) {
    throw new Error(
      `${destination}: expected ${expectedBytes} bytes, got ${bytes.length}. ` +
        `The upstream asset changed — verify it, then update the pin in this script.`,
    );
  }

  writeFileSync(destination, bytes);
  return bytes.length;
}

/**
 * Extract named entries from a ZIP buffer.
 *
 * Hand-rolled for the same reason `fetch-binaries.mjs` does it: "tar" on Windows
 * is whichever one is first on PATH — PowerShell provides bsdtar (reads ZIP),
 * Git Bash provides GNU tar (cannot, and parses `C:\path` as a remote host).
 * ZIP entries are raw deflate, which zlib already handles.
 */
function unzip(buf, wanted) {
  const EOCD_SIG = 0x06054b50;
  const CEN_SIG = 0x02014b50;

  let eocd = -1;
  for (let i = buf.length - 22; i >= Math.max(0, buf.length - 65557); i -= 1) {
    if (buf.readUInt32LE(i) === EOCD_SIG) {
      eocd = i;
      break;
    }
  }
  if (eocd === -1) throw new Error("not a ZIP archive");

  const count = buf.readUInt16LE(eocd + 10);
  let offset = buf.readUInt32LE(eocd + 16);
  const found = new Map();

  for (let i = 0; i < count; i += 1) {
    if (buf.readUInt32LE(offset) !== CEN_SIG) break;

    const method = buf.readUInt16LE(offset + 10);
    const compressedSize = buf.readUInt32LE(offset + 20);
    const nameLen = buf.readUInt16LE(offset + 28);
    const extraLen = buf.readUInt16LE(offset + 30);
    const commentLen = buf.readUInt16LE(offset + 32);
    const localOffset = buf.readUInt32LE(offset + 42);
    const name = buf.subarray(offset + 46, offset + 46 + nameLen).toString("utf8");
    const base = name.split("/").pop();

    if (wanted.includes(base)) {
      // The central directory's extra-field length may differ from the local
      // header's, so the data offset must come from the local header.
      const localNameLen = buf.readUInt16LE(localOffset + 26);
      const localExtraLen = buf.readUInt16LE(localOffset + 28);
      const start = localOffset + 30 + localNameLen + localExtraLen;
      const raw = buf.subarray(start, start + compressedSize);
      found.set(base, method === 0 ? raw : inflateRawSync(raw));
    }
    offset += 46 + nameLen + extraLen + commentLen;
  }
  return found;
}

// -------------------------------------------------------------------- main

async function main() {
  const argv = process.argv.slice(2);

  if (argv.includes("--list")) {
    console.log("Accuracy tiers:\n");
    for (const [id, tier] of Object.entries(TIERS)) {
      console.log(`  ${id.padEnd(9)} ${tier.note}`);
    }
    console.log(`\nAll three total ${mb(Object.values(TIERS).reduce((n, t) => n + t.bytes, 0))}.`);
    return;
  }

  const host = `${process.platform}-${process.arch}`;
  mkdirSync(MODEL_DEST, { recursive: true });

  // --- binary ------------------------------------------------------------

  const spec = BINARIES[host];
  if (!spec) {
    console.warn(
      `! No prebuilt whisper.cpp for ${host}. Install one on your PATH — on macOS,\n` +
        `  \`brew install whisper-cpp\` — and the app will find it there.`,
    );
  } else if (existsSync(join(DEST, spec.keep[0]))) {
    console.log(`  = ${spec.keep[0]} already vendored`);
  } else {
    const tmp = join(DEST, "whisper.zip");
    await download(spec.archive, tmp);

    const entries = unzip(readFileSync(tmp), spec.keep);
    rmSync(tmp, { force: true });

    const missing = spec.keep.filter((name) => !entries.has(name));
    if (missing.length) {
      throw new Error(
        `The whisper.cpp archive did not contain: ${missing.join(", ")}. ` +
          `Its layout changed — check the ${WHISPER_TAG} release.`,
      );
    }

    let total = 0;
    for (const [name, data] of entries) {
      writeFileSync(join(DEST, name), data);
      total += data.length;
    }
    console.log(`  ✓ whisper.cpp ${WHISPER_TAG} — ${entries.size} files (${mb(total)})`);
  }

  if (argv.includes("--binary-only")) {
    writeManifest();
    return;
  }

  // --- models ------------------------------------------------------------

  const requested = argv.includes("--tier")
    ? argv[argv.indexOf("--tier") + 1].split(",").map((s) => s.trim())
    : Object.keys(TIERS);

  for (const id of requested) {
    if (!TIERS[id]) {
      throw new Error(`Unknown tier "${id}". Run with --list to see the options.`);
    }
  }

  for (const id of requested) {
    const tier = TIERS[id];
    const path = join(MODEL_DEST, tier.file);

    if (existsSync(path) && statSync(path).size === tier.bytes) {
      console.log(`  = ${tier.file} already vendored (${mb(tier.bytes)})`);
      continue;
    }

    console.log(`Fetching ${id} — ${tier.note}`);
    const size = await download(`${HF}/${tier.file}`, path, tier.bytes);
    console.log(`  ✓ ${tier.file} (${mb(size)})`);
  }

  writeManifest();
}

/**
 * Record what is actually present.
 *
 * The backend re-derives this by listing the directory — the manifest exists so
 * a human can see what a given build carries without measuring file sizes.
 */
function writeManifest() {
  const present = Object.entries(TIERS)
    .filter(([, tier]) => existsSync(join(MODEL_DEST, tier.file)))
    .map(([id, tier]) => ({
      id,
      file: tier.file,
      bytes: statSync(join(MODEL_DEST, tier.file)).size,
    }));

  writeFileSync(
    join(DEST, "manifest.json"),
    `${JSON.stringify({ whisper: WHISPER_TAG, tiers: present }, null, 2)}\n`,
  );

  const total = present.reduce((n, t) => n + t.bytes, 0);
  console.log(
    present.length
      ? `  ✓ manifest.json — tiers: ${present.map((t) => t.id).join(", ")} (${mb(total)})`
      : `  ! no models vendored; the Audio → SRT tab will say so`,
  );
  console.log("\nAudio → SRT is ready — transcription runs locally, no network at runtime.");
}

main().catch((error) => {
  console.error(`\n✗ ${error.message}`);
  process.exit(1);
});
