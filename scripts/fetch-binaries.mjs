/**
 * Optional dev-only vendoring of yt-dlp, ffmpeg, and ffprobe.
 *
 * These three are NOT bundled with the app any more — the shipped build
 * resolves them from the user's own PATH (see `src-tauri/src/binaries.rs`), so
 * this script is not part of `npm run setup` and is not needed to produce a
 * release. It exists for one case: a dev machine with no system FFmpeg or
 * yt-dlp that still wants `tauri dev` and the smoke tests to run. `binaries.rs`
 * checks `src-tauri/binaries/` after PATH, so whatever lands here is picked up
 * as a fallback and never shadows a real install.
 *
 *   node scripts/fetch-binaries.mjs                              # host target
 *   node scripts/fetch-binaries.mjs --target aarch64-apple-darwin
 *   node scripts/fetch-binaries.mjs --target universal-apple-darwin
 *   node scripts/fetch-binaries.mjs --all-mac                    # both mac arches
 *
 * Files are named `<name>-<triple><ext>` — the layout Tauri's `externalBin`
 * used to require, and still what the dev-tree lookup expects. They go to
 * `src-tauri/binaries/` and are gitignored: build inputs, not source.
 *
 * The macOS binaries are statically linked against system libraries only,
 * verified by inspecting the Mach-O load commands (see `verifyMachO`).
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
import { createGunzip, inflateRawSync } from "node:zlib";
import { Readable } from "node:stream";
import { pipeline } from "node:stream/promises";
import { createWriteStream } from "node:fs";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const DEST = join(ROOT, "src-tauri", "binaries");

/** Pinned so a vendored build is reproducible; bump deliberately. */
const FFMPEG_STATIC_TAG = "b6.1.1";
const FFMPEG_STATIC = `https://github.com/eugeneware/ffmpeg-static/releases/download/${FFMPEG_STATIC_TAG}`;
const BTBN = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest";
const YTDLP = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";

// ---------------------------------------------------------------- targets

/**
 * Everything platform-specific in one table.
 *
 * `ffmpeg` is either an archive to unpack, or `direct` binaries to download
 * as-is. macOS uses eugeneware/ffmpeg-static because BtbN publishes no macOS
 * build and Homebrew's ffmpeg is dynamically linked against /opt/homebrew,
 * which would break the moment the app left this machine.
 */
const TARGETS = {
  "x86_64-pc-windows-msvc": {
    exe: ".exe",
    ytdlp: "yt-dlp.exe",
    ffmpeg: { kind: "archive", url: `${BTBN}/ffmpeg-master-latest-win64-gpl.zip`, file: "ffmpeg.zip" },
  },
  "aarch64-pc-windows-msvc": {
    exe: ".exe",
    ytdlp: "yt-dlp.exe",
    ffmpeg: { kind: "archive", url: `${BTBN}/ffmpeg-master-latest-winarm64-gpl.zip`, file: "ffmpeg.zip" },
  },
  "x86_64-apple-darwin": {
    exe: "",
    ytdlp: "yt-dlp_macos", // universal2
    ffmpeg: { kind: "direct", suffix: "darwin-x64" },
  },
  "aarch64-apple-darwin": {
    exe: "",
    ytdlp: "yt-dlp_macos",
    ffmpeg: { kind: "direct", suffix: "darwin-arm64" },
  },
  "universal-apple-darwin": {
    exe: "",
    ytdlp: "yt-dlp_macos",
    // No universal build is published, so fuse the two thin ones with lipo.
    ffmpeg: { kind: "lipo", suffixes: ["darwin-arm64", "darwin-x64"] },
  },
  "x86_64-unknown-linux-gnu": {
    exe: "",
    ytdlp: "yt-dlp_linux",
    ffmpeg: { kind: "archive", url: `${BTBN}/ffmpeg-master-latest-linux64-gpl.tar.xz`, file: "ffmpeg.tar.xz" },
  },
  "aarch64-unknown-linux-gnu": {
    exe: "",
    ytdlp: "yt-dlp_linux_aarch64",
    ffmpeg: { kind: "archive", url: `${BTBN}/ffmpeg-master-latest-linuxarm64-gpl.tar.xz`, file: "ffmpeg.tar.xz" },
  },
};

/** Ask rustc for the host triple; fall back to a platform/arch mapping. */
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
    throw new Error(
      `Unsupported host ${process.platform}-${process.arch}. Install Rust so rustc can report the triple.`,
    );
  }
  console.warn(`! rustc not found — assuming ${fallback}`);
  return fallback;
}

// --------------------------------------------------------------- utilities

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

/** Download a .gz and expand it in one pass — the mac binaries ship gzipped. */
async function downloadGz(url, destination) {
  process.stdout.write(`  ↓ ${url.split("/").pop()}\n`);
  const response = await fetch(url, { redirect: "follow" });
  if (!response.ok) {
    throw new Error(`HTTP ${response.status} ${response.statusText} for ${url}`);
  }
  await pipeline(
    Readable.fromWeb(response.body),
    createGunzip(),
    createWriteStream(destination),
  );
}

/**
 * Extract named entries from a ZIP buffer.
 *
 * Hand-rolled rather than shelled out to `tar`, because "tar" on Windows is
 * whichever one is first on PATH: PowerShell gets bsdtar (reads zip), but a Git
 * Bash shell gets GNU tar, which cannot read zip at all and additionally parses
 * `C:\path` as a remote `host:path`. ZIP entries are raw deflate, which zlib
 * already handles, so this needs no dependency.
 *
 * Only standard (non-Zip64) archives are supported — the FFmpeg builds are
 * ~150 MB with a handful of entries, far below the 4 GB / 65535-entry limits.
 */
function unzipEntries(buf, wanted) {
  const EOCD_SIG = 0x06054b50;
  const CEN_SIG = 0x02014b50;

  // The end-of-central-directory record sits within the last 64 KB.
  let eocd = -1;
  for (let i = buf.length - 22; i >= Math.max(0, buf.length - 65557); i -= 1) {
    if (buf.readUInt32LE(i) === EOCD_SIG) {
      eocd = i;
      break;
    }
  }
  if (eocd === -1) throw new Error("not a ZIP archive (no end-of-central-directory record)");

  const count = buf.readUInt16LE(eocd + 10);
  let offset = buf.readUInt32LE(eocd + 16);
  if (offset === 0xffffffff) throw new Error("Zip64 archives are not supported");

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

      if (method === 0) {
        found.set(base, Buffer.from(raw));
      } else if (method === 8) {
        found.set(base, inflateRawSync(raw));
      } else {
        throw new Error(`${name} uses unsupported compression method ${method}`);
      }
    }

    offset += 46 + nameLen + extraLen + commentLen;
  }

  return found;
}

/**
 * Unpack a .tar.xz. Only Linux uses this format, where GNU tar handles it
 * natively. Paths are passed relative with `cwd` to sidestep the `host:path`
 * parsing described above.
 */
function untar(archiveName, into) {
  const result = spawnSync("tar", ["-xf", archiveName], { cwd: into, encoding: "utf8" });
  if (result.status !== 0) {
    const detail = (result.stderr ?? result.error?.message ?? "").trim();
    throw new Error(
      `\`tar -xf ${archiveName}\` failed in ${into}: ${detail || "unknown error"}\n` +
        "  .tar.xz requires xz-utils to be installed.",
    );
  }
}

function findFile(root, name) {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const full = join(root, entry.name);
    if (entry.isDirectory()) {
      const nested = findFile(full, name);
      if (nested) return nested;
    } else if (entry.name === name) {
      return full;
    }
  }
  return null;
}

/**
 * Confirm a macOS binary is the expected architecture and links nothing outside
 * /usr/lib and /System — i.e. that "bundled" is actually true.
 */
function verifyMachO(path) {
  const buf = readFileSync(path);
  const CPU = { 0x0100000c: "arm64", 0x01000007: "x86_64" };

  const readThin = (offset) => {
    const magic = buf.readUInt32LE(offset);
    if (magic !== 0xfeedfacf) return null;
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
      ? Array.from({ length: buf.readUInt32BE(4) }, (_, i) =>
          readThin(buf.readUInt32BE(8 + i * 20 + 8)),
        )
      : [readThin(0)];

  const arches = [];
  for (const s of slices) {
    if (!s) continue;
    arches.push(s.arch);
    const foreign = s.dylibs.filter(
      (d) => !d.startsWith("/usr/lib/") && !d.startsWith("/System/"),
    );
    if (foreign.length) {
      throw new Error(
        `${path} links non-system libraries and would not run on another Mac: ${foreign.join(", ")}`,
      );
    }
  }
  return arches;
}

function install(sourcePath, stem, triple) {
  const spec = TARGETS[triple];
  const target = join(DEST, `${stem}-${triple}${spec.exe}`);
  copyFileSync(sourcePath, target);
  if (process.platform !== "win32") chmodSync(target, 0o755);

  let note = "";
  if (triple.includes("apple-darwin")) {
    try {
      note = `  [${verifyMachO(target).join(" + ")}, system libs only]`;
    } catch (e) {
      // A bad binary must not be silently shipped.
      rmSync(target, { force: true });
      throw e;
    }
    adHocSign(target);
  }

  const size = (statSync(target).size / (1024 * 1024)).toFixed(1);
  console.log(`  ✓ ${stem}-${triple}${spec.exe} (${size} MB)${note}`);
}

/**
 * Apple Silicon refuses to execute a binary with no code signature at all.
 * Downloaded binaries usually carry an ad-hoc one already, but re-signing is
 * cheap insurance and a no-op when it is unnecessary. Only possible on macOS.
 */
function adHocSign(path) {
  if (process.platform !== "darwin") return;
  const r = spawnSync("codesign", ["--force", "--sign", "-", path], { encoding: "utf8" });
  if (r.status !== 0) {
    console.warn(`  ! codesign failed for ${path}: ${(r.stderr ?? "").trim()}`);
  }
}

// -------------------------------------------------------------------- steps

async function installYtDlp(triple, scratch) {
  const spec = TARGETS[triple];
  console.log("yt-dlp:");
  const temp = join(scratch, spec.ytdlp);
  await download(`${YTDLP}/${spec.ytdlp}`, temp);
  install(temp, "yt-dlp", triple);
}

async function installFfmpeg(triple, scratch) {
  const { ffmpeg } = TARGETS[triple];
  console.log("ffmpeg + ffprobe:");

  if (ffmpeg.kind === "direct") {
    for (const name of ["ffmpeg", "ffprobe"]) {
      const temp = join(scratch, `${name}-${ffmpeg.suffix}`);
      await downloadGz(`${FFMPEG_STATIC}/${name}-${ffmpeg.suffix}.gz`, temp);
      install(temp, name, triple);
    }
    return;
  }

  if (ffmpeg.kind === "lipo") {
    if (process.platform !== "darwin") {
      throw new Error(
        "universal-apple-darwin needs `lipo`, which only exists on macOS.\n" +
          "  Build per-architecture instead:\n" +
          "    node scripts/fetch-binaries.mjs --target aarch64-apple-darwin\n" +
          "    node scripts/fetch-binaries.mjs --target x86_64-apple-darwin",
      );
    }
    for (const name of ["ffmpeg", "ffprobe"]) {
      const parts = [];
      for (const suffix of ffmpeg.suffixes) {
        const temp = join(scratch, `${name}-${suffix}`);
        await downloadGz(`${FFMPEG_STATIC}/${name}-${suffix}.gz`, temp);
        parts.push(temp);
      }
      const fused = join(scratch, `${name}-universal`);
      const r = spawnSync("lipo", ["-create", ...parts, "-output", fused], { stdio: "inherit" });
      if (r.status !== 0) throw new Error(`lipo failed for ${name}`);
      install(fused, name, triple);
    }
    return;
  }

  const exe = TARGETS[triple].exe;
  const unpacked = join(scratch, "unpacked");
  mkdirSync(unpacked, { recursive: true });

  if (ffmpeg.file.endsWith(".zip")) {
    process.stdout.write(`  ↓ ${ffmpeg.url.split("/").pop()}\n`);
    const response = await fetch(ffmpeg.url, { redirect: "follow" });
    if (!response.ok) {
      throw new Error(`HTTP ${response.status} ${response.statusText} for ${ffmpeg.url}`);
    }
    const zip = Buffer.from(await response.arrayBuffer());

    const wanted = ["ffmpeg", "ffprobe"].map((n) => `${n}${exe}`);
    const entries = unzipEntries(zip, wanted);

    for (const name of ["ffmpeg", "ffprobe"]) {
      const data = entries.get(`${name}${exe}`);
      if (!data) throw new Error(`${name}${exe} not found inside ${ffmpeg.file}`);
      const temp = join(unpacked, `${name}${exe}`);
      writeFileSync(temp, data);
      install(temp, name, triple);
    }
    return;
  }

  // Download into the extraction directory so tar only sees a bare filename.
  await download(ffmpeg.url, join(unpacked, ffmpeg.file));
  untar(ffmpeg.file, unpacked);

  for (const name of ["ffmpeg", "ffprobe"]) {
    const found = findFile(unpacked, `${name}${exe}`);
    if (!found) throw new Error(`${name}${exe} not found inside ${ffmpeg.file}`);
    install(found, name, triple);
  }
}

async function vendor(triple) {
  if (!TARGETS[triple]) {
    throw new Error(
      `Unknown target \`${triple}\`. Supported:\n  ${Object.keys(TARGETS).join("\n  ")}`,
    );
  }
  console.log(`\n── ${triple} ──`);
  const scratch = mkdtempSync(join(tmpdir(), "mk-binaries-"));
  try {
    await installYtDlp(triple, scratch);
    await installFfmpeg(triple, scratch);
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
  if (argv.includes("--all-mac")) {
    targets.push("aarch64-apple-darwin", "x86_64-apple-darwin");
  }
  if (targets.length === 0) targets.push(hostTriple());

  mkdirSync(DEST, { recursive: true });
  for (const triple of targets) {
    await vendor(triple);
  }

  const present = readdirSync(DEST).filter((f) => f !== ".gitkeep");
  console.log(`\nDone — ${present.length} file(s) in src-tauri/binaries/`);

  if (!existsSync(join(ROOT, "app-icon.png"))) {
    console.log("Reminder: run `npm run icon` before the first `tauri build`.");
  }
}

main().catch((error) => {
  console.error(`\n✗ ${error.message}`);
  process.exit(1);
});
