/**
 * The single boundary between the UI and the Rust backend.
 *
 * Every `invoke` is wrapped so that a rejected promise always surfaces as a
 * well-formed `ErrorPayload`, whether it came from our `AppError` serialiser or
 * from Tauri itself (e.g. a permission denial).
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type { PlatformId } from "./platforms";
import type {
  ErrorPayload,
  JobEvent,
  SliceRequest,
  ToolchainStatus,
  VideoInfo,
} from "./types";

const PROGRESS_EVENT = "mk://job-progress";

/** Coerce anything thrown across the IPC boundary into an `ErrorPayload`. */
export function toErrorPayload(value: unknown): ErrorPayload {
  if (value && typeof value === "object" && "kind" in value && "message" in value) {
    const e = value as Record<string, unknown>;
    return {
      kind: String(e.kind),
      message: String(e.message),
      hint: typeof e.hint === "string" ? e.hint : null,
    };
  }
  return {
    kind: "unknown",
    message: value instanceof Error ? value.message : String(value),
    hint: null,
  };
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(command, args);
  } catch (raw) {
    throw toErrorPayload(raw);
  }
}

export const api = {
  checkToolchain: () => call<ToolchainStatus>("check_toolchain"),

  defaultOutputDir: () => call<string>("default_output_dir"),

  suggestFilename: (title: string, start: number, end: number, extension: string) =>
    call<string>("suggest_filename", { title, start, end, extension }),

  /** `platform` decides which link shapes the backend will accept. */
  analyzeUrl: (url: string, platform: PlatformId, cookies: SliceRequest["cookies"]) =>
    call<VideoInfo>("analyze_url", { url, platform, cookies }),

  startSlice: (request: SliceRequest) => call<string>("start_slice", { request }),

  cancelJob: (jobId: string) => call<boolean>("cancel_job", { jobId }),

  /**
   * Write frontend-produced bytes to a path the user picked in a save dialog.
   *
   * Base64 rather than a byte array: Tauri's IPC would otherwise serialise the
   * buffer as a JSON array of numbers, which for a multi-megabyte PNG is both
   * several times larger on the wire and slow to parse on both ends.
   */
  writeFile: (path: string, bytes: Uint8Array) =>
    call<void>("write_file", { path, contentsBase64: toBase64(bytes) }),

  revealInFolder: (path: string) => call<void>("reveal_in_folder", { path }),
};

/**
 * Base64-encode without blowing the argument limit.
 *
 * `String.fromCharCode(...bytes)` is the usual one-liner and throws on anything
 * past a few hundred kilobytes, so the buffer is walked in chunks instead.
 */
function toBase64(bytes: Uint8Array): string {
  const CHUNK = 0x8000;
  let binary = "";
  for (let i = 0; i < bytes.length; i += CHUNK) {
    binary += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
  }
  return btoa(binary);
}

/**
 * Subscribe to backend job progress.
 *
 * Returns the unlisten handle; the caller owns it. Events for jobs other than
 * the caller's are filtered out upstream by comparing `jobId`.
 */
export function onJobProgress(handler: (event: JobEvent) => void): Promise<UnlistenFn> {
  return listen<JobEvent>(PROGRESS_EVENT, (message) => handler(message.payload));
}
