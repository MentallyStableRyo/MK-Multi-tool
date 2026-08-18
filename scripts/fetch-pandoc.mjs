/**
 * Vendors Pandoc, the document-conversion engine behind the General Converter's
 * Documents category (TXT, Markdown, HTML, DOCX, ODT, RTF, EPUB).
 *
 *   node scripts/fetch-pandoc.mjs                              # host target
 *   node scripts/fetch-pandoc.mjs --target aarch64-apple-darwin
 *   node scripts/fetch-pandoc.mjs --all-mac
 *
 * Pandoc ships as a single self-contained executable on every platform it
 * publishes for (no sibling DLLs, unlike whisper.cpp), so it slots into the
 * same `<name>-<triple><ext>` sidecar convention as yt-dlp/ffmpeg/ffprobe and
 * is resolved by the same `binaries::locate` — see `fetch-binaries.mjs` for the
 * full rationale on that naming scheme and why ZIP/tar extraction is
 * hand-rolled here rather than shelling out to a platform `tar`.
 *
 * Windows ships only an x86_64 build upstream (no arm64), so
 * `aarch64-pc-windows-msvc` has no vendored Pandoc; that target falls back to
 * a `pandoc` on PATH, exactly like whisper.cpp does on macOS.
 */

import { spawnSync } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { inflateRawSync } from "node:zlib";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const DEST = join(ROOT, "src-tauri", "binaries");

/** Pinned so a vendored build is reproducible; bump deliberately. */
const PANDOC_VERSION = "3.10.2";
const RELEASE = `https://github.com/jgm/pandoc/releases/download/${PANDOC_VERSION}`;

/**
 * Per-target archive and the path to `pandoc` inside it once unpacked.
 *
 * The macOS zips name their top-level folder after the *asset* arch label
 * (`x86_64` / `arm64`), not the Rust triple, so that is spelled out per entry
 * rather than derived.
 */
const TARGETS = {
  "x86_64-pc-windows-msvc": {
    exe: ".exe",
    kind: "zip",
    url: `${RELEASE}/pandoc-${PANDOC_VERSION}-windows-x86_64.zip`,
    inner: `pandoc-${PANDOC_VERSION}/pandoc.exe`,
  },
  "x86_64-apple-darwin": {
    exe: "",
    kind: "zip",
    url: `${RELEASE}/pandoc-${PANDOC_VERSION}-x86_64-macOS.zip`,
    inner: `pandoc-${PANDOC_VERSION}-x86_64/bin/pandoc`,
  },
  "aarch64-apple-darwin": {
    exe: "",
    kind: "zip",
    url: `${RELEASE}/pandoc-${PANDOC_VERSION}-arm64-macOS.zip`,
    inner: `pandoc-${PANDOC_VERSION}-arm64/bin/pandoc`,
  },
  "universal-apple-darwin": {
    exe: "",
    kind: "lipo",
    of: ["aarch64-apple-darwin", "x86_64-apple-darwin"],
  },
  "x86_64-unknown-linux-gnu": {
    exe: "",
    kind: "tar",
    url: `${RELEASE}/pandoc-${PANDOC_VERSION}-linux-amd64.tar.gz`,
    inner: `pandoc-${PANDOC_VERSION}/bin/pandoc`,
  },
  "aarch64-unknown-linux-gnu": {
    exe: "",
    kind: "tar",
    url: `${RELEASE}/pandoc-${PANDOC_VERSION}-linux-arm64.tar.gz`,
    inner: `pandoc-${PANDOC_VERSION}/bin/pandoc`,
  },
};

// --------------------------------------------------------------- utilities
// (host-triple detection, ZIP extraction, and Mach-O verification mirror
// fetch-binaries.mjs exactly — see there for why each is implemented this way.)

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
  if (!fallback) {
    throw new Error(`Unsupported host ${process.platform}-${process.arch}.`);
  }
  return fallback;
}

async function download(url, destination) {
  process.stdout.write(`  ↓ ${url.split("/").pop()}\n`);
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) {
    throw new Error(`HTTP ${response.status} ${response.statusText} for ${url}`);
  }
  const bytes = Buffer.from(await response.arrayBuffer());
  writeFileSync(destination, bytes);
  return bytes.length;
}

function unzipEntry(buf, wantedPath) {
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

  for (let i = 0; i < count; i += 1) {
    if (buf.readUInt32LE(offset) !== CEN_SIG) break;
    const method = buf.readUInt16LE(offset + 10);
    const compressedSize = buf.readUInt32LE(offset + 20);
    const nameLen = buf.readUInt16LE(offset + 28);
    const extraLen = buf.readUInt16LE(offset + 30);
    const commentLen = buf.readUInt16LE(offset + 32);
    const localOffset = buf.readUInt32LE(offset + 42);
    const name = buf.subarray(offset + 46, offset + 46 + nameLen).toString("utf8");

    if (name === wantedPath) {
      const localNameLen = buf.readUInt16LE(localOffset + 26);
      const localExtraLen = buf.readUInt16LE(localOffset + 28);
      const start = localOffset + 30 + localNameLen + localExtraLen;
      const raw = buf.subarray(start, start + compressedSize);
      return method === 0 ? Buffer.from(raw) : inflateRawSync(raw);
    }
    offset += 46 + nameLen + extraLen + commentLen;
  }
  return null;
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
    throw new Error(
      `\`tar -xzf\` failed: ${(result.stderr ?? result.error?.message ?? "").trim()}`,
    );
  }
}

function verifyMachO(path) {
  const buf = readFileSync(path);
  const CPU = { 0x0100000c: "arm64", 0x01000007: "x86_64" };
  const readThin = (offset) => {
    if (buf.readUInt32LE(offset) !== 0xfeedfacf) return null;
    const ncmds = buf.readUInt32LE(offset + 16);
    let p = offset + 32;
    const dylibs = [];
    for (let i = 0; i < ncmds; i += 1) {
      const cmd = buf.readUInt32LE(p);
      const size = buf.readUInt32LE(p + 4);
      if (!size || p + size > buf.length) break;
      if (cmd === 0x0c || cmd === 0x80000018) {
        const so = buf.readUInt32LE(p + 8);
        const raw = buf.subarray(p + so, p + size);
        const end = raw.indexOf(0);
        dylibs.push(raw.subarray(0, end === -1 ? raw.length : end).toString());
      }
      p += size;
    }
    return { arch: CPU[buf.readUInt32LE(offset + 4)] ?? "?", dylibs };
  };
  const slices =
    buf.readUInt32BE(0) === 0xcafebabe
      ? Array.from({ length: buf.readUInt32BE(4) }, (_, i) => readThin(buf.readUInt32BE(8 + i * 20 + 8)))
      : [readThin(0)];
  for (const s of slices) {
    if (!s) continue;
    const foreign = s.dylibs.filter((d) => !d.startsWith("/usr/lib/") && !d.startsWith("/System/"));
    if (foreign.length) {
      throw new Error(`${path} links non-system libraries: ${foreign.join(", ")}`);
    }
  }
}

function adHocSign(path) {
  if (process.platform !== "darwin") return;
  spawnSync("codesign", ["--force", "--sign", "-", path], { encoding: "utf8" });
}

function install(sourcePath, triple) {
  const spec = TARGETS[triple];
  const target = join(DEST, `pandoc-${triple}${spec.exe}`);
  copyFileSync(sourcePath, target);
  if (process.platform !== "win32") chmodSync(target, 0o755);
  if (triple.includes("apple-darwin")) {
    verifyMachO(target);
    adHocSign(target);
  }
  const size = (statSync(target).size / (1024 * 1024)).toFixed(1);
  console.log(`  ✓ pandoc-${triple}${spec.exe} (${size} MB)`);
}

// -------------------------------------------------------------------- steps

async function vendor(triple) {
  const spec = TARGETS[triple];
  if (!spec) {
    throw new Error(`Unknown target \`${triple}\`. Supported:\n  ${Object.keys(TARGETS).join("\n  ")}`);
  }
  console.log(`\n── pandoc: ${triple} ──`);

  if (existsSync(join(DEST, `pandoc-${triple}${spec.exe}`))) {
    console.log(`  = already vendored`);
    return;
  }

  const scratch = mkdtempSync(join(tmpdir(), "mk-pandoc-"));
  try {
    if (spec.kind === "lipo") {
      if (process.platform !== "darwin") {
        throw new Error("universal-apple-darwin needs `lipo`, which only exists on macOS.");
      }
      const parts = [];
      for (const sub of spec.of) {
        await vendor(sub);
        parts.push(join(DEST, `pandoc-${sub}`));
      }
      const fused = join(scratch, "pandoc-universal");
      const r = spawnSync("lipo", ["-create", ...parts, "-output", fused], { stdio: "inherit" });
      if (r.status !== 0) throw new Error("lipo failed for pandoc");
      install(fused, triple);
      return;
    }

    if (spec.kind === "zip") {
      const zipPath = join(scratch, "pandoc.zip");
      await download(spec.url, zipPath);
      const data = unzipEntry(readFileSync(zipPath), spec.inner);
      if (!data) throw new Error(`${spec.inner} not found inside the archive — layout may have changed.`);
      const out = join(scratch, `pandoc${spec.exe}`);
      writeFileSync(out, data);
      install(out, triple);
      return;
    }

    // tar.gz
    await download(spec.url, join(scratch, "pandoc.tar.gz"));
    untar("pandoc.tar.gz", scratch);
    const found = join(scratch, spec.inner);
    if (!existsSync(found)) throw new Error(`${spec.inner} not found after extraction.`);
    install(found, triple);
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

  if (targets.includes("aarch64-pc-windows-msvc")) {
    console.warn(
      "! Pandoc publishes no arm64 Windows build. The app will fall back to a `pandoc` on PATH there.",
    );
  }

  mkdirSync(DEST, { recursive: true });
  for (const triple of targets) {
    if (triple === "aarch64-pc-windows-msvc") continue;
    await vendor(triple);
  }

  console.log(
    "\nDone. Documents (TXT/Markdown/HTML/DOCX/ODT/RTF/EPUB) are ready in the General Converter tab.",
  );
}

main().catch((error) => {
  console.error(`\n✗ ${error.message}`);
  process.exit(1);
});
