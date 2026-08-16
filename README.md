# MK Multi-tool

A native desktop app that takes a link from **YouTube, Instagram, TikTok, or X**,
lets you pick an exact time range, and renders just that slice to disk. Tauri +
Rust backend, no server, no uploads — every byte moves between the site's CDN and
your machine.

It also carries two tabs that never touch the network at all: **Remove
Background**, which cuts the subject out of an image, and **Audio → SRT**, which
transcribes audio or video into subtitles. Both run their models locally.

---

## One tab per platform

Each platform gets its own tab, and each tab is a self-contained workspace: its
own link, its own analysis, its own slice, its own render. Switching tabs never
discards anything, and a render started in one tab keeps running (and keeps
reporting, marked by a dot on its tab) while you set up another.

| Tab       | Accepted links                                                     |
| --------- | ------------------------------------------------------------------ |
| YouTube   | `watch?v=`, `youtu.be/`, `/shorts/`, `/live/`, `/embed/`, bare IDs  |
| Instagram | `/p/`, `/reel/`, `/tv/`, `/share/…`                                 |
| TikTok    | `/@user/video/…`, `vm.tiktok.com/…`, `vt.tiktok.com/…`, `/t/…`      |
| X         | `x.com/user/status/…`, `twitter.com/user/status/…`                  |

Links are validated per tab (`src-tauri/src/platform.rs`) before anything is
spawned, so a link pasted into the wrong tab is rejected with a message that
names the tab rather than becoming an opaque extractor error later. Paste a link
that belongs to another platform and the app simply switches to that tab and
analyses it there.

Everything past link normalisation is site-agnostic: yt-dlp flattens all four
sites into one metadata shape, so the quality model, the cut planner, and the
FFmpeg pipeline are shared.

---

## Remove Background

The fifth tab, sitting past a divider because it is not a video source. Drop a
PNG, JPG, or WEBP onto it — or pick one with the file dialog — and it cuts the
subject out and hands back a transparent PNG.

It runs entirely on your machine. A saliency model executes in WebAssembly inside
the app's own window via ONNX Runtime; the image never leaves the process, and
neither does anything else.

Two models ship, switchable per image from the Result card:

| Quality | Model | Mask | Time | 3 px strands recovered |
| ------- | ---------------------- | ------ | ------ | ---------------------- |
| Precise | [IS-Net][isnet] general | 1024²  | ~10 s  | alpha **255**/255      |
| Fast    | [U²-Net][u2net] lite    | 320²   | ~1.4 s | alpha **33**/255       |

**Precise is the default**, because the difference is not subtle. The numbers
above are measured on a 1200×900 scene with 3 px strands: a 320² mask carries one
mask pixel per ~4 image pixels, so hair, fur, and foliage smear away no matter
how the alpha is post-processed. Fast is there for when the subject is a blob on
a plain background and ten seconds is not worth it.

Switching quality re-cuts the image already loaded, leaving the previous result
on screen while it works, so the two are directly comparable.

[u2net]: https://github.com/xuebinqin/U-2-Net
[isnet]: https://github.com/xuebinqin/DIS

**Nothing is downloaded at runtime.** The model and the runtime are vendored into
`public/` at setup time by `npm run model`, and resolved from the app's own
origin. Three things hold that line:

- The `.wasm` is handed to ONNX Runtime as a buffer, so no URL is built for it.
- The loader `.mjs` is pinned to an absolute same-origin URL rather than a prefix
  the runtime completes itself.
- `connect-src` in the CSP (`src-tauri/tauri.conf.json`) is limited to `'self'`
  and the IPC endpoints, so a request to anywhere else fails regardless.

The tab is lazy in every sense: the runtime is a dynamic import in its own chunk,
each model's weights are read on first use of *that* quality, and the manifest —
the only thing needed to draw the quality toggle — is a few hundred bytes read
separately from the runtime. A session that never opens the tab touches none of
it, and one that stays on Fast never reads the 170 MB model at all.

**Inference runs on a worker, not the window's thread.** At 1024² it is a single
synchronous call holding the thread for ~11 seconds, during which nothing
repaints: the progress bar freezes and Windows ghosts the window as "not
responding". Measured, the longest main-thread block drops from **11,104 ms to
223 ms** with `env.wasm.proxy` enabled. This is not WASM *threading*, which would
need SharedArrayBuffer and therefore COOP/COEP on every response — and that would
break the YouTube embed the preview tab depends on. The proxy talks over
postMessage and leaves `numThreads` at 1.

Vendoring a different set is a re-run of the script rather than a code change:
`matte.ts` resolves the tensor names off the session and reads each model's input
size and normalisation constants from the manifest. The two share none of those
numbers — IS-Net takes 1024² centred on 0.5, U²-Net takes 320² with ImageNet
statistics, and each rejects the other's input shape — so they travel per-model
in `fetch-model.mjs`, and only tiers whose weights are actually present are
listed. Vendor just one and the toggle does not appear.

```bash
npm run model                            # both tiers (default)
node scripts/fetch-model.mjs --model u2netp   # fast only — small download
node scripts/fetch-model.mjs --list
```

Images larger than 4096 px on the long edge are scaled to fit, and the app says
so — the mask carries no more detail than that, and the full size would cost far
more memory.

---

## Audio → SRT

The sixth tab. Point it at audio — or at video, which FFmpeg strips the audio
out of first — and it writes a SubRip `.srt` beside it. [whisper.cpp][wcpp] runs
the speech model as a local binary, the same way yt-dlp and FFmpeg are local
binaries. Nothing is uploaded.

[wcpp]: https://github.com/ggml-org/whisper.cpp

### Three accuracy tiers

| Tier | Model | Size | Speed | Decoder |
| ------------- | ---------------- | ------ | ------------------- | ------------------------- |
| Fast | base | 141 MB | ~7× realtime | greedy, no fallback |
| Normal | small | 465 MB | ~2× realtime | beam 5 |
| Very accurate | large-v3-turbo | 1.5 GB | slower than realtime| beam 8, loosened thresholds |

The tiers differ in two ways at once, because model size alone leaves accuracy on
the table. A larger model reads harder audio; a wider beam search reconsiders
more candidate transcriptions before committing. The slowest tier also loosens
the entropy and log-probability thresholds so the decoder *retries* a failing
window at a higher temperature instead of emitting its first plausible guess —
which is where most hallucinated text and dropped words come from.

All three are multilingual rather than the `.en` variants, so auto-detect works;
an English-only model would silently mistranscribe everything else.

### Words per caption

A slider from 1 to 20, defaulting to 7. It is a **ceiling, not a quota**: a
caption also ends at a sentence boundary once it is at least half full, at a
silence longer than 900 ms, and at 7 seconds on screen. A subtitle that runs past
its sentence, or hangs through a pause, reads worse than a short one — but
nothing ever exceeds the number you set.

This is why the grouping works off whisper's *tokens* rather than its text.
Tokens are BPE fragments — `" coun"`, `"try"`, `","` — and a token that begins a
new word carries a leading space. Reassembling words from that, instead of
splitting the segment text on whitespace, is what keeps each word's own start and
end timestamps attached to it. Splitting on whitespace would leave every caption
sharing one segment-wide timespan.

Captions the model was unsure of are flagged in the preview, since those are the
ones worth checking by ear.

### Setup

```bash
npm run whisper                                    # binary + all three tiers (~2.1 GB)
# The installer ships only fast+normal; see "Why the largest model is not in the installer"
node scripts/fetch-whisper.mjs --tier fast,normal  # skip the large model
node scripts/fetch-whisper.mjs --binary-only
node scripts/fetch-whisper.mjs --list
```

Tiers whose model is absent are shown greyed out rather than failing when
pressed, so a build carrying only Fast is a coherent build.

### Why the largest model is not in the installer

Neither Windows installer format can carry it. Measured, not assumed — both were
built and both failed:

| Format | Failure | Cap |
| ------ | -------------------------------------------- | ----------------------- |
| MSI | `LGHT0306`, CAB finalization | 2 GB, the CAB format |
| NSIS | `error mmapping file (1955445654, 33554432)` | ~2 GB, 32-bit offsets |

The full payload is about 2.6 GB: 2.2 GB of whisper, 297 MB of FFmpeg and
yt-dlp, 186 MB of frontend. There is no configuration that fits that into one
Windows installer, and `msi` is off the target list entirely because it cannot
build at anything near this size.

So the installer ships **Fast and Normal** (~1.1 GB) and the 1.5 GB
`ggml-large-v3-turbo.bin` is side-loaded. `binaries.rs` searches the app-data
directory as well as the bundle, which is user-writable — unlike Program Files,
which needs elevation — and survives an upgrade. The tab names the exact folder,
shows the filename it wants, and offers a button to copy the path.

Development builds are unaffected: `npm run whisper` vendors all three into
`src-tauri/binaries/whisper/models/`, and `tauri dev` resolves them from there.

whisper.cpp does **not** ship through Tauri's `externalBin` like the other
sidecars do. `externalBin` renames single files and drops them next to the
executable, and `whisper-cli` dynamically loads `whisper.dll` plus a set of
`ggml-cpu-*` backends chosen at runtime from the host's instruction set — so it
only works with its siblings beside it. The whole directory travels as a bundle
resource instead, and `binaries.rs` resolves that layout. On macOS, where
whisper.cpp publishes no prebuilt binary, `brew install whisper-cpp` puts one on
`PATH` and the same resolver finds it.

---

## How it works

The interesting part is what the app *doesn't* do: it never downloads the whole
video.

```
yt-dlp  ──▶  direct CDN stream URLs + auth headers
                       │
ffprobe ──▶  keyframe timestamps around your cut point   (one ranged request)
                       │
ffmpeg  ──▶  -ss <keyframe> -i <url>  …  -t <length>  ──▶  <name>.mk-part
                                                              │
                                        atomic rename ────────▶  your file
```

`-ss` placed **before** `-i` makes FFmpeg issue HTTP range requests, so cutting
90:00–92:00 out of a three-hour video transfers about two minutes of media, not
three hours. yt-dlp is used purely as a *resolver* — it produces the URLs, and
our own FFmpeg pipeline does all the cutting.

### Copy vs. re-encode

Stream-copying is lossless and effectively instant, but it's only valid when the
cut lands on a keyframe. Rather than guessing, the app reads the real keyframe
layout before deciding:

| Situation                                  | What happens                    |
| ------------------------------------------ | ------------------------------- |
| Keyframe at the cut, codec fits container   | `-c:v copy` — lossless, instant |
| Cut falls between keyframes                 | Hardware-accelerated re-encode  |
| Codec illegal in the container (VP9 → MP4)  | Hardware-accelerated re-encode  |

### Reviewing the cut before rendering — YouTube only

The **Preview** card sits directly above the slice controls, because you decide
where to cut by watching. It plays the selected range in an embedded player:

- **Play slice** — the whole range, start to finish.
- **Start scene** / **End scene** — the first and last few seconds, for checking
  exactly where the cut lands.

Nothing is downloaded to preview; it streams straight from YouTube.

The preview exists **only in the YouTube tab**. It is not a feature that was
switched off elsewhere — Instagram, TikTok, and X have no embeddable player the
app can seek, so anything shown there would be a still image pretending to be
one. `platforms.ts` marks which platforms have a real player, and the card is
built only for those.

**The cursor follows the handles, live.** Drag either end of the slice and the
player seeks with it, frame by frame, with no reload and no black frame. That
works because the embed is driven over its own `postMessage` protocol
(`enablejsapi=1`, a `listening` handshake, then `seekTo` commands) rather than
through YouTube's IFrame Player API — which would mean admitting a remote script
into the app's origin through the CSP, when the API is only a wrapper over that
same protocol. Seeks are coalesced to one per animation frame and rate-limited
beyond that, so a 120 Hz drag does not outrun the player.

The player reports its position back, which the timeline draws as a playhead,
and which is used to stop playback on the *exact* end you picked — more precise
than the `end` URL parameter, which only accepts whole seconds.

The protocol is undocumented, so the path degrades: if the embed never answers
the handshake, the preview falls back to whole-second `start`/`end` parameters
and marks itself stale when the range moves, exactly as it behaved before.

### AV1 → H.264

AV1 is legal in MP4 and MKV, so an AV1 track is stream-copied by default —
lossless and instant. But older TVs, phones, and editors cannot decode it.
**Advanced → Compatibility → Convert AV1 to H.264** forces a re-encode instead,
trading the lossless copy for playability.

The option is ignored where it cannot apply: it does nothing on a non-AV1 track,
and it is disabled for WebM (which cannot store H.264) and for audio-only
containers. The hint beneath it says which case you are in.

### Cut mode

Cut mode is selectable under **Advanced**:

- **Auto** (default) — copy when the cut is keyframe-aligned, re-encode otherwise.
- **Exact** — always frame-accurate; always re-encodes the video.
- **Fast** — never re-encode; snaps the start back to the preceding keyframe.

Hardware encoders are probed at runtime (NVENC → Quick Sync → AMF on Windows,
VideoToolbox on macOS, NVENC → Quick Sync on Linux) and the render automatically
falls back to libx264 if the GPU path fails — which happens in practice on
laptops whose discrete GPU is powered down.

---

## Prerequisites

### 1. Rust toolchain

```bash
# All platforms — https://rustup.rs
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

On Windows, download and run [rustup-init.exe](https://rustup.rs) instead.

### 2. Platform build dependencies

**Windows**

- [Microsoft C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
  with the "Desktop development with C++" workload.
- WebView2 — preinstalled on Windows 11 and current Windows 10.

**macOS**

```bash
xcode-select --install
```

**Linux (Wayland and X11 both work — GTK picks the backend automatically)**

```bash
# Debian / Ubuntu
sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev

# Fedora
sudo dnf install webkit2gtk4.1-devel openssl-devel curl wget file \
  libappindicator-gtk3-devel librsvg2-devel libxdo-devel

# Arch
sudo pacman -S webkit2gtk-4.1 base-devel curl wget file openssl \
  libayatana-appindicator librsvg libxdo
```

### 3. Node.js 18+

Used only for the frontend build and the setup scripts.

---

## Setup

```bash
npm install

# Vendor yt-dlp + ffmpeg + ffprobe for your host into src-tauri/binaries/
npm run binaries

# Vendor the segmentation models + ONNX Runtime into public/ (Remove Background)
# ~175 MB; use `--model u2netp` for the 4.4 MB fast tier alone
npm run model

# Vendor whisper.cpp + speech models into src-tauri/binaries/ (Audio → SRT)
# ~2.1 GB; use `--tier fast` for the 141 MB tier alone
npm run whisper

# Generate app-icon.png and expand it into every platform icon format
npm run icon
```

`npm run setup` runs the icon, binary, and model steps in one go.

`npm run binaries` downloads statically linked builds and names them
`<tool>-<target-triple><ext>`, which is what Tauri's `externalBin` requires. The
files are gitignored — they're build inputs, not source.

To vendor for a target other than your host:

```bash
node scripts/fetch-binaries.mjs --target aarch64-apple-darwin
node scripts/fetch-binaries.mjs --all-mac     # both macOS arches at once
```

Supported: `x86_64`/`aarch64-pc-windows-msvc`, `x86_64`/`aarch64-apple-darwin`,
`universal-apple-darwin`, `x86_64`/`aarch64-unknown-linux-gnu`.

The script has no dependencies and unpacks archives itself — ZIP entries are
inflated in-process rather than shelled out to `tar`, because "tar" on Windows
is whichever one is first on `PATH`: PowerShell provides bsdtar (reads ZIP),
while Git Bash provides GNU tar (cannot read ZIP, and parses `C:\path` as a
remote host). Both shells are tested.

At runtime the app also falls back to `PATH`, so a machine with ffmpeg and
yt-dlp already installed works without vendoring.

`npm run model` is the only step that fetches the Remove Background assets, and
it is also the only network access that feature ever involves. Skipping it leaves
the other four tabs working; the Remove Background tab then reports the missing
file and names this command.

---

## Development

```bash
npm run dev
```

Starts Vite on `127.0.0.1:5173` and launches the app with hot reload on the
frontend. Rust changes trigger a recompile and relaunch.

```bash
cd src-tauri && cargo test      # backend unit tests (42, all offline)
npx tsc --noEmit                # frontend typecheck
cargo clippy --all-targets      # lints
```

There is also a network-backed end-to-end test that drives the real pipeline
against a real video — resolve, keyframe probe, render, verify — and asserts the
rendered slice is the requested length and that progress reaches 100%. It is
`#[ignore]`d so a flaky network can never fail a normal test run:

```bash
cd src-tauri && cargo test -- --ignored --nocapture
```

Run it after touching `youtube.rs`, `platform.rs`, `render.rs`, `probe.rs`, or `pipeline.rs`,
and whenever a YouTube-side change is suspected. It exercises
`pipeline::execute` — the exact code path the app uses, retries included —
rather than a reimplementation of it.

---

## Building release binaries

```bash
npm run build
```

Artifacts land in `src-tauri/target/release/bundle/`.

### Windows

```bash
npm run build
# → bundle/msi/MK Multi-tool_0.1.0_x64_en-US.msi
# → bundle/nsis/MK Multi-tool_0.1.0_x64-setup.exe
```

### macOS — both architectures

**This must run on a Mac.** Tauri cannot cross-compile a macOS bundle from
Windows or Linux: linking Apple targets needs the macOS SDK and Apple linker,
and `.app`/`.dmg` packaging needs `codesign` and `hdiutil`. There is no
workaround.

Everything *else* is ready — the sidecars are vendored and verified from any
host, so on the Mac it is three commands:

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin

npm install
npm run icon
npm run binaries:mac        # static ffmpeg/ffprobe + universal yt-dlp, both arches

npm run build:mac           # Apple Silicon  → .app + .dmg
npm run build:mac-intel     # Intel
npm run build:mac-universal # one binary containing both (needs `lipo`)
```

Artifacts land in `src-tauri/target/<triple>/release/bundle/{macos,dmg}/`.

#### What gets bundled, and why it is genuinely self-contained

`npm run binaries:mac` fetches **statically linked** binaries and refuses to
install one that is not:

| Binary    | Source                        | Arch          | Size    |
| --------- | ----------------------------- | ------------- | ------- |
| `yt-dlp`  | yt-dlp `yt-dlp_macos`         | universal2    | 36.5 MB |
| `ffmpeg`  | `eugeneware/ffmpeg-static`    | per-arch      | 43–75 MB |
| `ffprobe` | `eugeneware/ffmpeg-static`    | per-arch      | 43–75 MB |

The script parses each Mach-O binary's load commands and **fails the vendoring
step** if anything links outside `/usr/lib` and `/System`. This matters: an
earlier version copied ffmpeg from `PATH`, which on a dev machine means
Homebrew's build — dynamically linked against `/opt/homebrew/lib/*.dylib` and
therefore broken on every machine but the one that built it. All six binaries
were verified to link system frameworks only.

`VideoToolbox.framework` is among them, so hardware-accelerated H.264 encoding
(`h264_videotoolbox`) works on Apple Silicon rather than falling back to libx264.

Sidecars land in `Contents/MacOS/` beside the app binary, which is the first
place the resolver looks — no `PATH` dependency at runtime.

#### Minimum macOS version

`minimumSystemVersion` is **12.0**, and that is not arbitrary: the arm64 ffmpeg
build declares `LC_BUILD_VERSION minos=12.0`. Lowering it would let the app
install on macOS 11 where ffmpeg cannot launch. (The Intel binaries and yt-dlp
go lower — 10.13 and 11.0 — so 12.0 is set by the arm64 ffmpeg alone.)

#### Signing

Apple Silicon refuses to execute a binary with no code signature, so the
vendoring script ad-hoc signs each sidecar (`codesign -s -`) when run on macOS.
That is enough to run locally.

To distribute to other people, sign and notarize properly — Tauri signs the
sidecars too when an identity is present:

```bash
export APPLE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)"
export APPLE_ID="you@example.com"
export APPLE_PASSWORD="app-specific-password"   # app-specific, not your Apple ID password
export APPLE_TEAM_ID="TEAMID"
npm run build:mac-universal
```

Without a Developer ID, Gatekeeper shows *"cannot be opened because the developer
cannot be verified"*. Right-click → Open, or `xattr -dr com.apple.quarantine
"/Applications/MK Multi-tool.app"`.

### Linux

```bash
npm run build
# → bundle/deb/mk-multitool_0.1.0_amd64.deb
# → bundle/rpm/mk-multitool-0.1.0-1.x86_64.rpm
# → bundle/appimage/mk-multitool_0.1.0_amd64.AppImage
```

Wayland and X11 are both handled by WebKitGTK at runtime — one build covers
both. If compositing artifacts appear under Wayland on older drivers:

```bash
WEBKIT_DISABLE_COMPOSITING_MODE=1 ./mk-multitool
```

For ARM64:

```bash
rustup target add aarch64-unknown-linux-gnu
npm run build -- --target aarch64-unknown-linux-gnu
```

---

## Restricted videos

Private, members-only, and age-restricted videos need a signed-in session. Open
**Advanced** and either pick a browser under *Browser cookies* — the app reads
that browser's cookie store locally — or point *cookies.txt* at a
Netscape-format export.

On Instagram this is closer to mandatory than optional: it serves almost nothing
to a signed-out client, and the app reports that as a `login_required` failure
with the same remedy.

TikTok is stricter still. It fingerprints the request and will serve a captcha
page instead of the video to any client it doesn't recognise — reported as
`bot_check`. This is not something the app can route around: it fails
identically on the current stable *and* nightly yt-dlp, through the webpage, the
embed, and every `api_hostname` the extractor accepts. Borrowing a browser
session that has already cleared the check is the fix; a different network or a
VPN is the fallback.

Nothing is uploaded. Cookies are read on your machine and handed straight to the
yt-dlp subprocess.

Geo-blocked videos cannot be worked around by the app; that requires a VPN.

---

## Project layout

```
src/                    Frontend (TypeScript, no framework — ~57 KB bundled)
  main.ts               App shell: tab strip, shared chrome, boot
  tool.ts               The tab contract, and the tab strip's order
  view.ts               One platform's workspace — four isolated instances
  platforms.ts          The platform table: labels, icons, link rules, preview
  preview.ts            The live YouTube player, driven over postMessage
  removebg.ts           The Remove Background workspace
  matte.ts              Local segmentation: lazy ONNX Runtime, U²-Net, compositing
  transcribe.ts         The Audio → SRT workspace
  bridge.ts             The only place that talks to Rust
  timeline.ts           Dual-handle range scrubber with a live playhead
  time.ts               Timecode parsing/formatting, mirrors timecode.rs
  types.ts              Mirrors of the serde types

src-tauri/src/
  commands.rs           Tauri command surface — validate, spawn, translate events
  pipeline.rs           The slice workflow: resolve → plan → render → commit
  platform.rs           Per-site link rules, one per tab
  youtube.rs            yt-dlp resolution and the quality model
  render.rs             Cut planning and the FFmpeg invocation
  transcribe.rs         Audio → SRT: decode, run whisper.cpp, group captions
  srt.rs                Tokens → words → captions → SRT (pure, unit-tested)
  probe.rs              ffprobe keyframe and duration queries
  disk.rs               Free-space verification
  staging.rs            Atomic output commits
  jobs.rs               Cancellation registry and progress events
  proc.rs               Cancellation-aware child-process plumbing
  ffargs.rs             Shared FFmpeg input-argument construction
  binaries.rs           Sidecar discovery
  timecode.rs           Range validation
  error.rs              Typed errors with actionable hints
  smoke.rs              Network-backed end-to-end test (ignored by default)

scripts/
  fetch-binaries.mjs    Vendors sidecars for the host triple
  fetch-model.mjs       Vendors the segmentation model + ONNX Runtime into public/
  fetch-whisper.mjs     Vendors whisper.cpp + speech models into src-tauri/binaries/
  gen-icon.mjs          Generates app-icon.png (hand-rolled PNG encoder)

public/                 Gitignored build inputs, served from the app's own origin
  models/               The ONNX models and the manifest describing them
  ort/                  ONNX Runtime WebAssembly and its loader
```

Dependencies only ever point downward. Nothing below `commands.rs` knows what a
window is, which is what keeps the media pipeline unit-testable without Tauri.

---

## Design notes

**Why yt-dlp instead of `rusty_ytdl`.** YouTube rotates its signature cipher,
throttling parameter, and anti-automation checks every few weeks. yt-dlp ships
fixes within days; a pure-Rust extractor pinned in `Cargo.lock` silently rots and
starts returning 403s. Running it as a subprocess also means a hang in the
extractor cannot take the UI down, and the binary updates independently of the
app.

**Stream URLs are re-resolved at render time.** The URLs captured during analysis
are short-lived and IP-bound, so a stale set would 403 partway through a render.

**Background removal runs in the webview, not in Rust.** The `ort` crate needs
its own native ONNX Runtime resolved at build time, which would make every
contributor's first `cargo build` depend on a 200 MB download and complicate all
three release targets. WebAssembly costs perhaps 2× the inference time on an
operation that already takes a second, and buys a pure-npm dependency, no change
to the bundling story, and a model that loads lazily by construction. The image
never crosses the IPC boundary; only the finished PNG does, on export.

**The frontend's one write path is a command, not a filesystem plugin.**
`commands.rs` is "the only place the frontend can reach", and adding
`tauri-plugin-fs` would widen that to a general scoped filesystem API for the
sake of one PNG. `write_file` takes a path the user already chose in the system
save dialog. The bytes travel base64-encoded because Tauri's IPC serialises a
`Vec<u8>` as a JSON array of decimal numbers — several times larger on the wire,
and slow to parse at both ends.

**Three attempts on transient CDN failures.** Google's media CDN intermittently
answers a perfectly valid, unexpired URL with `403 Forbidden`, then serves the
same URL seconds later. This is not rare: during development the end-to-end test
needed two retries on one run and one on another. Each attempt re-resolves
through yt-dlp for a fresh URL batch, which is why `MAX_ATTEMPTS` is 3 — two
would have failed a run we actually observed.

**Progress is measured in frames, not muxer position.** FFmpeg's `-progress`
reports `out_time` as the furthest-muxed packet across *all* streams. When video
is re-encoded while audio is stream-copied, the copied audio races ahead and
`out_time` overshoots the real output length by ~50% (measured: 15.1s reported
for a 10.0s file). The `frame` counter excludes frames discarded by an
output-side seek and tracks linearly, so progress uses `frame / (duration × fps)`
and falls back to `out_time` only for audio-only outputs, where there is one
stream and no discrepancy.

**The output muxer is named explicitly.** Renders target a `<name>.mk-part`
staging file, which FFmpeg cannot map to a format by extension — it aborts with
"Unable to choose an output format". Every render passes `-f <muxer>`.

**Output is atomic.** Renders write to `<name>.mk-part` beside the destination
and are renamed into place only on success — a same-volume rename, so it's
instant. A cancelled or failed job never leaves a half-written file that looks
playable until you scrub to the end.

**Cancellation is real.** Every child process is spawned with `kill_on_drop` and
raced against a `CancellationToken`. Cancelling, closing the window, or quitting
kills the encoder immediately instead of orphaning it.
