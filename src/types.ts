/**
 * Mirrors of the serde types in `src-tauri/src`.
 *
 * Rust serialises with `rename_all = "camelCase"`, so these field names must
 * match exactly. Keep this file in lockstep with the backend structs.
 */

import type { PlatformId } from "./platforms";

export interface ToolStatus {
  name: string;
  found: boolean;
  path: string | null;
  version: string | null;
}

export interface ToolchainStatus {
  ready: boolean;
  tools: ToolStatus[];
}

/** One selectable output quality. `id` is a yt-dlp format selector. */
export interface QualityTrack {
  id: string;
  label: string;
  detail: string;
  height: number;
  fps: number;
  videoCodec: string;
  audioCodec: string;
  ext: string;
  bitrateKbps: number;
  hdr: boolean;
  audioOnly: boolean;
  /** Both streams can be copied straight into an MP4 with no re-encode. */
  mp4Ready: boolean;
}

export interface VideoInfo {
  id: string;
  title: string;
  uploader: string;
  thumbnail: string | null;
  durationSeconds: number;
  webpageUrl: string;
  isLive: boolean;
  viewCount: number | null;
  uploadDate: string | null;
  tracks: QualityTrack[];
}

export type Container = "mp4" | "mkv" | "webm" | "m4a" | "mp3";
export type Precision = "auto" | "exact" | "fast";

export interface CookieOptions {
  browser?: string | null;
  file?: string | null;
}

export interface SliceRequest {
  url: string;
  /** The tab this came from; decides how `url` is validated. */
  platform: PlatformId;
  trackId: string;
  start: number;
  end: number;
  durationSeconds: number;
  container: Container;
  precision: Precision;
  /** Re-encode AV1 sources to H.264 instead of stream-copying them. */
  transcodeAv1: boolean;
  outputPath: string;
  /** Only used for the pre-flight disk-space estimate. */
  bitrateKbps: number;
  title: string;
  cookies: CookieOptions | null;
}

export type Stage =
  | "resolving"
  | "planning"
  | "rendering"
  | "finalizing"
  | "done"
  | "failed"
  | "cancelled";

/** Serialised `AppError` — every rejected `invoke` resolves to this shape. */
export interface ErrorPayload {
  kind: string;
  message: string;
  hint: string | null;
}

export interface JobEvent {
  jobId: string;
  stage: Stage;
  message: string;
  percent: number | null;
  speed: string | null;
  etaSeconds: number | null;
  bytesWritten: number | null;
  outputPath: string | null;
  error: ErrorPayload | null;
  lossless: boolean | null;
}

/** Containers that discard the video stream. */
export const AUDIO_CONTAINERS: ReadonlySet<Container> = new Set<Container>(["m4a", "mp3"]);

// ------------------------------------------------------------- transcription

/** How hard the speech model works. Mirrors `transcribe::Accuracy`. */
export type Accuracy = "fast" | "normal" | "accurate";

/** One tier, and whether its model is actually installed. */
export interface TierStatus {
  id: Accuracy;
  label: string;
  model: string;
  available: boolean;
  /** Rough multiple of audio duration this tier takes to run. */
  realtimeFactor: number;
  /**
   * Writable folder to drop a missing model into.
   *
   * Neither Windows installer format can carry the largest model — both cap
   * below 2 GB — so it is side-loaded here rather than bundled.
   */
  installDir: string | null;
}

export interface TranscribeRequest {
  inputPath: string;
  accuracy: Accuracy;
  wordsPerCaption: number;
  /** BCP-47-ish code, or `auto` to detect. */
  language: string;
  translate: boolean;
  outputPath: string;
}

export interface PreviewCaption {
  index: number;
  start: string;
  end: string;
  text: string;
  confidence: number;
}

/** Emitted on `mk://transcript-ready` when a transcription finishes. */
export interface TranscriptSummary {
  captions: number;
  words: number;
  language: string;
  durationMs: number;
  confidence: number;
  preview: PreviewCaption[];
  outputPath: string;
}

/** Bounds the backend clamps `wordsPerCaption` to. */
export const MIN_WORDS_PER_CAPTION = 1;
export const MAX_WORDS_PER_CAPTION = 20;
