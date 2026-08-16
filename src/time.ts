/**
 * Timecode helpers.
 *
 * These mirror `src-tauri/src/timecode.rs` so the UI can validate instantly
 * without a round-trip. The backend re-validates everything regardless — this
 * is for responsiveness, not for safety.
 */

/** Format seconds as `M:SS` / `H:MM:SS`, the compact form used in inputs. */
export function formatClock(seconds: number, forceHours = false): string {
  const total = Math.max(0, Math.floor(seconds));
  const s = total % 60;
  const m = Math.floor(total / 60) % 60;
  const h = Math.floor(total / 3600);

  const pad = (n: number) => n.toString().padStart(2, "0");
  return h > 0 || forceHours ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

/** Format with centiseconds, for readouts where precision matters. */
export function formatPrecise(seconds: number): string {
  const clamped = Math.max(0, seconds);
  const centis = Math.round((clamped % 1) * 100)
    .toString()
    .padStart(2, "0");
  return `${formatClock(Math.floor(clamped))}.${centis}`;
}

/**
 * Parse `SS`, `MM:SS`, or `HH:MM:SS`, with an optional fractional last part.
 * Returns `null` for anything malformed — callers decide how to report it.
 */
export function parseClock(input: string): number | null {
  const text = input.trim();
  if (text === "") return null;

  const parts = text.split(":");
  if (parts.length > 3) return null;

  let seconds = 0;
  for (let i = 0; i < parts.length; i += 1) {
    const raw = (parts[i] ?? "").trim();
    if (raw === "" || !/^\d+(\.\d+)?$/.test(raw)) return null;

    const value = Number(raw);
    if (!Number.isFinite(value)) return null;

    // Only the final segment may carry a fraction.
    const isLast = i === parts.length - 1;
    if (!isLast && !Number.isInteger(value)) return null;

    seconds = seconds * 60 + value;
  }
  return seconds;
}

/** Human byte count with one decimal above the KB threshold. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  const value = bytes / 1024 ** exponent;
  return `${exponent === 0 ? value.toFixed(0) : value.toFixed(1)} ${units[exponent]}`;
}

/** Compact duration for ETA readouts: `42s`, `3m 05s`, `1h 12m`. */
export function formatEta(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "—";
  if (seconds < 60) return `${Math.ceil(seconds)}s`;

  const m = Math.floor(seconds / 60);
  const s = Math.round(seconds % 60);
  if (m < 60) return `${m}m ${s.toString().padStart(2, "0")}s`;

  const h = Math.floor(m / 60);
  return `${h}h ${(m % 60).toString().padStart(2, "0")}m`;
}

/** Estimated output size in bytes for a track bitrate over a duration. */
export function estimateSize(bitrateKbps: number, seconds: number): number {
  if (bitrateKbps <= 0 || seconds <= 0) return 0;
  return (bitrateKbps * 1000 * seconds) / 8;
}

export interface RangeError {
  field: "start" | "end";
  message: string;
}

/** Validate a slice range against the source duration. */
export function validateRange(
  start: number,
  end: number,
  duration: number,
): RangeError | null {
  if (start < 0) return { field: "start", message: "Start cannot be negative" };
  if (duration > 0 && start >= duration) {
    return { field: "start", message: `Start is past the end (${formatClock(duration)})` };
  }
  if (end <= start) return { field: "end", message: "End must be after start" };
  if (duration > 0 && end > duration + 0.5) {
    return { field: "end", message: `End exceeds the video length (${formatClock(duration)})` };
  }
  if (end - start < 0.1) return { field: "end", message: "Slice is too short" };
  return null;
}
