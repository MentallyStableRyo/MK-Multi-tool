/**
 * Dual-handle range scrubber.
 *
 * Built on pointer events with pointer capture rather than two overlaid
 * `<input type="range">` elements: capture keeps a drag alive when the cursor
 * leaves the window, which is exactly the case that makes stacked native
 * sliders drop the gesture.
 */

import { formatClock } from "./time";

export type Handle = "start" | "end";

export interface TimelineCallbacks {
  /**
   * Fired continuously while dragging. `moving` is the handle under the
   * cursor, which is what the preview binds its own cursor to.
   */
  onInput: (start: number, end: number, moving: Handle) => void;
  /** Fired when a gesture begins, before it has moved anything. */
  onScrubStart?: (moving: Handle) => void;
  /** Fired once when a drag finishes, for work too costly to do per-frame. */
  onCommit: (start: number, end: number) => void;
}

export class Timeline {
  private readonly root: HTMLElement;
  private readonly track: HTMLElement;
  private readonly startHandle: HTMLElement;
  private readonly endHandle: HTMLElement;
  private readonly scale: HTMLElement;
  private readonly playhead: HTMLElement;
  private readonly callbacks: TimelineCallbacks;

  private duration = 0;
  private start = 0;
  private end = 0;
  private dragging: Handle | null = null;
  private enabled = false;

  constructor(root: HTMLElement, callbacks: TimelineCallbacks) {
    this.root = root;
    this.callbacks = callbacks;

    root.classList.add("timeline");
    root.innerHTML = `
      <div class="tl-track"></div>
      <div class="tl-selection"></div>
      <div class="tl-playhead" hidden></div>
      <div class="tl-handle start" role="slider" tabindex="0" aria-label="Start time"></div>
      <div class="tl-handle end" role="slider" tabindex="0" aria-label="End time"></div>
      <div class="tl-scale"><span>0:00</span><span>0:00</span></div>
    `;

    this.track = root.querySelector(".tl-track") as HTMLElement;
    this.startHandle = root.querySelector(".tl-handle.start") as HTMLElement;
    this.endHandle = root.querySelector(".tl-handle.end") as HTMLElement;
    this.scale = root.querySelector(".tl-scale") as HTMLElement;
    this.playhead = root.querySelector(".tl-playhead") as HTMLElement;

    this.startHandle.addEventListener("pointerdown", (e) => this.beginDrag(e, "start"));
    this.endHandle.addEventListener("pointerdown", (e) => this.beginDrag(e, "end"));
    this.track.addEventListener("pointerdown", (e) => this.jumpNearest(e));

    for (const handle of [this.startHandle, this.endHandle]) {
      handle.addEventListener("keydown", (e) => this.nudge(e));
    }
  }

  /** Smallest allowed gap between handles, scaled to the video length. */
  private get minGap(): number {
    return Math.max(0.1, this.duration * 0.0005);
  }

  setDuration(duration: number): void {
    this.duration = Math.max(0, duration);
    this.enabled = this.duration > 0;
    this.root.style.opacity = this.enabled ? "1" : "0.4";
    this.root.style.pointerEvents = this.enabled ? "auto" : "none";
    if (!this.enabled) this.setPlayhead(null);
    this.paint();
  }

  /** Update the handles without emitting callbacks (for external edits). */
  setRange(start: number, end: number): void {
    this.start = start;
    this.end = end;
    this.paint();
  }

  /**
   * Mark where the preview player is sitting, or `null` to hide the marker.
   *
   * Written straight to a custom property rather than through `paint()`: this
   * arrives several times a second from the player and must not drag the rest
   * of the widget — or any of the app's state — along with it.
   */
  setPlayhead(seconds: number | null): void {
    if (seconds === null || !this.enabled) {
      this.playhead.hidden = true;
      return;
    }
    this.playhead.hidden = false;
    this.playhead.style.left = `${this.ratio(seconds) * 100}%`;
  }

  private ratio(seconds: number): number {
    if (this.duration <= 0) return 0;
    return Math.min(1, Math.max(0, seconds / this.duration));
  }

  private paint(): void {
    this.root.style.setProperty("--sel-start", `${this.ratio(this.start) * 100}%`);
    this.root.style.setProperty("--sel-end", `${this.ratio(this.end) * 100}%`);

    const [left, right] = this.scale.children;
    if (left) left.textContent = formatClock(this.start);
    if (right) right.textContent = formatClock(this.duration);

    for (const [handle, value] of [
      [this.startHandle, this.start],
      [this.endHandle, this.end],
    ] as const) {
      handle.setAttribute("aria-valuemin", "0");
      handle.setAttribute("aria-valuemax", String(Math.round(this.duration)));
      handle.setAttribute("aria-valuenow", String(Math.round(value)));
      handle.setAttribute("aria-valuetext", formatClock(value));
    }
  }

  /** Convert a pointer x-coordinate into a timestamp on the track. */
  private secondsAt(clientX: number): number {
    const rect = this.track.getBoundingClientRect();
    if (rect.width <= 0) return 0;
    const ratio = Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
    return ratio * this.duration;
  }

  private beginDrag(event: PointerEvent, handle: Handle): void {
    if (!this.enabled) return;
    event.preventDefault();
    event.stopPropagation();

    this.dragging = handle;
    const element = handle === "start" ? this.startHandle : this.endHandle;
    element.classList.add("dragging");
    element.setPointerCapture(event.pointerId);
    this.callbacks.onScrubStart?.(handle);

    const move = (e: PointerEvent) => this.applyDrag(this.secondsAt(e.clientX));
    const finish = () => {
      element.classList.remove("dragging");
      element.releasePointerCapture(event.pointerId);
      element.removeEventListener("pointermove", move);
      element.removeEventListener("pointerup", finish);
      element.removeEventListener("pointercancel", finish);
      this.dragging = null;
      this.callbacks.onCommit(this.start, this.end);
    };

    element.addEventListener("pointermove", move);
    element.addEventListener("pointerup", finish);
    element.addEventListener("pointercancel", finish);
  }

  private applyDrag(seconds: number): void {
    const moving = this.dragging;
    if (moving === "start") {
      this.start = Math.min(Math.max(0, seconds), this.end - this.minGap);
    } else if (moving === "end") {
      this.end = Math.max(Math.min(this.duration, seconds), this.start + this.minGap);
    } else {
      return;
    }
    this.paint();
    this.callbacks.onInput(this.start, this.end, moving);
  }

  /** Clicking the bare track moves whichever handle is closer. */
  private jumpNearest(event: PointerEvent): void {
    if (!this.enabled) return;
    const seconds = this.secondsAt(event.clientX);
    const nearStart = Math.abs(seconds - this.start) <= Math.abs(seconds - this.end);

    this.dragging = nearStart ? "start" : "end";
    this.callbacks.onScrubStart?.(this.dragging);
    this.applyDrag(seconds);
    this.dragging = null;
    this.callbacks.onCommit(this.start, this.end);
  }

  /** Arrow keys nudge by 1s, Shift for 10s — the usual editor convention. */
  private nudge(event: KeyboardEvent): void {
    if (!this.enabled) return;
    const step = event.shiftKey ? 10 : 1;
    const direction = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
    if (direction === 0) return;

    event.preventDefault();
    const target = event.currentTarget as HTMLElement;
    this.dragging = target.classList.contains("start") ? "start" : "end";
    this.callbacks.onScrubStart?.(this.dragging);
    const current = this.dragging === "start" ? this.start : this.end;
    this.applyDrag(current + direction * step);
    this.dragging = null;
    this.callbacks.onCommit(this.start, this.end);
  }
}
