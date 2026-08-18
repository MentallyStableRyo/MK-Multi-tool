/**
 * Vendors PDFium — Google's PDF engine, used for the General Converter's PDF ↔
 * Image pages (PDF → images, images → PDF, and best-effort PDF text
 * extraction).
 *
 *   node scripts/fetch-pdfium.mjs                              # host target
 *   node scripts/fetch-pdfium.mjs --target aarch64-apple-darwin
 *   node scripts/fetch-pdfium.mjs --all-mac
 *
 * Unlike yt-dlp/ffmpeg/pandoc, PDFium is a *library*, not an executable — the
 * `pdfium-render` crate `dlopen`s it at runtime rather than spawning it, so it
 * cannot go through Tauri's `externalBin` (which is for renaming and running
 * programs). It travels as a bundle resource instead, the same way whisper.cpp's
 * directory does; `binaries::locate_pdfium` resolves it.
 *
 * Only the non-V8 build is vendored: the V8 variant links a JavaScript engine
 * for interactive PDF forms, which this app never executes — rendering pages
 * and extracting text need none of it, and it would roughly quadruple the
 * library's size for nothing used.
 *
 * bblanchon/pdfium-binaries publishes prebuilt, permissively licensed (BSD-3)
 * PDFium binaries for every desktop target Tauri builds for, refreshed from
 * Chromium regularly — see the vendored `LICENSE`/`licenses/` files copied
 * alongside it for the full attribution.
 */

import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const DEST = join(ROOT, "src-tauri", "binaries");

/** Pinned so a vendored build is reproducible; bump deliberately. */
const PDFIUM_TAG = "chromium%2F8009";
const RELEASE = `https://github.com/bblanchon/pdfium-binaries/releases/download/${PDFIUM_TAG}`;

/** Per-target archive and the path to the shared library inside it. */
const TARGETS = {
  "x86_64-pc-windows-msvc": { asset: "pdfium-win-x64.tgz", inner: "bin/pdfium.dll", out: "pdfium.dll" },
  "aarch64-pc-windows-msvc": { asset: "pdfium-win-arm64.tgz", inner: "bin/pdfium.dll", out: "pdfium.dll" },
  "x86_64-apple-darwin": { asset: "pdfium-mac-x64.tgz", inner: "lib/libpdfium.dylib", out: "libpdfium.dylib" },
  "aarch64-apple-darwin": { asset: "pdfium-mac-arm64.tgz", inner: "lib/libpdfium.dylib", out: "libpdfium.dylib" },
  "universal-apple-darwin": { lipo: ["aarch64-apple-darwin", "x86_64-apple-darwin"], out: "libpdfium.dylib" },
  "x86_64-unknown-linux-gnu": { asset: "pdfium-linux-x64.tgz", inner: "lib/libpdfium.so", out: "libpdfium.so" },
  "aarch64-unknown-linux-gnu": { asset: "pdfium-linux-arm64.tgz", inner: "lib/libpdfium.so", out: "libpdfium.so" },
};

// --------------------------------------------------------------- utilities

function hostTriple() {
  const rustc = spawnSync("rustc", ["-vV"], { encoding: "utf8" });
  if (rustc.status === 0) {
    const match = /^host:\s*(\S+)$/m.exec(rustc.stdout ?? "");
    if (match?.[1]) return match[1];
  }
  const fallback = {
    "win32-x64": "x86_64-pc-windows-msvc",
    "win32-arm64": "aarch64-pc-windows-msvc",
    "darwin-x64": "x86_64-apple-darwin",
    "darwin-arm64": "aarch64-apple-darwin",
    "linux-x64": "x86_64-unknown-linux-gnu",
    "linux-arm64": "aarch64-unknown-linux-gnu",
  }[`${process.platform}-${process.arch}`];
  if (!fallback) throw new Error(`Unsupported host ${process.platform}-${process.arch}.`);
  return fallback;
}

async function download(url, destination) {
  process.stdout.write(`  ↓ ${url.split("/").pop()}\n`);
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) throw new Error(`HTTP ${response.status} ${response.statusText} for ${url}`);
  writeFileSync(destination, Buffer.from(await response.arrayBuffer()));
}

/**
 * `archiveName` must be a bare filename, resolved via `cwd`, not a full path:
 * Git Bash's `tar` parses a leading `C:\...` drive letter as `host:path`
 * remote-shell syntax and fails with "Cannot connect to C: resolve failed" —
 * the same trap `fetch-binaries.mjs` documents and avoids the same way.
 */
function untar(archiveName, into) {
  const result = spawnSync("tar", ["-xzf", archiveName], { cwd: into, encoding: "utf8" });
  if (result.status !== 0) {
    throw new Error(`\`tar -xzf\` failed: ${(result.stderr ?? result.error?.message ?? "").trim()}`);
  }
}

function adHocSign(path) {
  if (process.platform !== "darwin") return;
  spawnSync("codesign", ["--force", "--sign", "-", path], { encoding: "utf8" });
}

// -------------------------------------------------------------------- steps

async function vendor(triple) {
  const spec = TARGETS[triple];
  if (!spec) {
    throw new Error(`Unknown target \`${triple}\`. Supported:\n  ${Object.keys(TARGETS).join("\n  ")}`);
  }
  console.log(`\n── pdfium: ${triple} ──`);

  const target = join(DEST, spec.out);
  // Universal and thin macOS builds share one output filename, so a stale thin
  // build must not be mistaken for the universal one requested afterwards.
  if (triple !== "universal-apple-darwin" && existsSync(target)) {
    console.log(`  = already vendored`);
    return;
  }

  const scratch = mkdtempSync(join(tmpdir(), "mk-pdfium-"));
  try {
    if (spec.lipo) {
      if (process.platform !== "darwin") {
        throw new Error("universal-apple-darwin needs `lipo`, which only exists on macOS.");
      }
      const parts = [];
      for (const sub of spec.lipo) {
        const subSpec = TARGETS[sub];
        const unpacked = join(scratch, sub);
        mkdirSync(unpacked, { recursive: true });
        await download(`${RELEASE}/${subSpec.asset}`, join(unpacked, "pdfium.tgz"));
        untar("pdfium.tgz", unpacked);
        parts.push(join(unpacked, subSpec.inner));
      }
      const fused = join(scratch, "libpdfium-universal.dylib");
      const r = spawnSync("lipo", ["-create", ...parts, "-output", fused], { stdio: "inherit" });
      if (r.status !== 0) throw new Error("lipo failed for pdfium");
      copyFileSync(fused, target);
    } else {
      await download(`${RELEASE}/${spec.asset}`, join(scratch, "pdfium.tgz"));
      untar("pdfium.tgz", scratch);
      const found = join(scratch, spec.inner);
      if (!existsSync(found)) throw new Error(`${spec.inner} not found inside ${spec.asset}.`);
      copyFileSync(found, target);
    }

    if (process.platform !== "win32") chmodSync(target, 0o755);
    if (triple.includes("apple-darwin")) adHocSign(target);

    const size = (statSync(target).size / (1024 * 1024)).toFixed(1);
    console.log(`  ✓ ${spec.out} (${size} MB)`);
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
}

// -------------------------------------------------------------------- main

async function main() {
  const argv = process.argv.slice(2);
  const targets = [];

  const targetFlag = argv.indexOf("--target");
  if (targetFlag !== -1) {
    const value = argv[targetFlag + 1];
    if (!value) throw new Error("--target needs a triple");
    targets.push(value);
  }
  if (argv.includes("--all-mac")) targets.push("aarch64-apple-darwin", "x86_64-apple-darwin");
  if (targets.length === 0) targets.push(hostTriple());

  mkdirSync(DEST, { recursive: true });
  for (const triple of targets) {
    await vendor(triple);
  }

  console.log("\nDone. PDF ↔ Image conversion is ready in the General Converter tab.");
}

main().catch((error) => {
  console.error(`\n✗ ${error.message}`);
  process.exit(1);
});
