/**
 * The YouTube review player.
 *
 * ## Why `postMessage` and not the IFrame Player API
 *
 * Seeking an embed by rewriting its `src` reloads the player: a black frame, a
 * fresh handshake with YouTube, and a second of buffering. That is fine for a
 * button, and useless for a cursor — dragging a handle would restart the video
 * dozens of times.
 *
 * The IFrame Player API solves it, but only by pulling `www.youtube.com/iframe_api`
 * into the page, which means widening `script-src` in the CSP to admit a remote
 * script into the app's own origin. That script is *itself* only a wrapper over
 * a `postMessage` protocol the embed already speaks, so this module speaks it
 * directly: `enablejsapi=1`, a `listening` handshake, then `seekTo` commands.
 * No remote code, no CSP change, and seeks that land in the same frame.
 *
 * The protocol is undocumented, so every path here degrades: if the widget
 * never answers the handshake, [`degraded`](Preview#degraded) goes true and the
 * caller falls back to reloading the frame when a drag ends.
 */

/** The privacy-preserving embed host, and the only origin we talk to. */
const EMBED_ORIGIN = "https://www.youtube-nocookie.com";

/** Channel identity for the widget protocol. Any stable string works. */
const WIDGET_ID = "mk-preview";

/** Handshake pacing: the widget ignores commands until it answers. */
const HANDSHAKE_INTERVAL_MS = 200;
const HANDSHAKE_ATTEMPTS = 25;

/**
 * Floor between seeks. A seek per `pointermove` (60–120 Hz) outruns the
 * player's own decoding and the picture stops updating entirely; ~12 Hz keeps
 * the frame changing continuously under the cursor.
 */
const SEEK_INTERVAL_MS = 80;

/** How long after a seek the auto-stop at the slice end stays suppressed. */
const SEEK_QUIET_MS = 600;

/** `YT.PlayerState.PLAYING`. */
const PLAYING = 1;

export interface PreviewCallbacks {
  /** The player's live position, or `null` once it has none. */
  onPlayhead: (seconds: number | null) => void;
  /** Whether the player is accepting live commands yet. */
  onStatus: (status: "connecting" | "live" | "degraded") => void;
}

export class Preview {
  private readonly stage: HTMLElement;
  private readonly placeholder: string;
  private readonly callbacks: PreviewCallbacks;

  private frame: HTMLIFrameElement | null = null;
  private videoId = "";
  /** Where playback should stop — the slice end, tracked live. */
  private stopAt = 0;

  /** The widget has answered and accepts commands. */
  private ready = false;
  /** The handshake was never answered; only reloads can move this player. */
  private fellBack = false;
  private handshakeTimer: number | null = null;
  private attempts = 0;

  private queued: number | null = null;
  private frameRequest: number | null = null;
  private lastSeekAt = 0;
  private scrubbing = false;
  private playing = false;

  constructor(stage: HTMLElement, placeholder: string, callbacks: PreviewCallbacks) {
    this.stage = stage;
    this.placeholder = placeholder;
    this.callbacks = callbacks;
    window.addEventListener("message", this.receive);
  }

  get open(): boolean {
    return this.frame !== null;
  }

  /** True once the handshake has been given up on — the caller must reload. */
  get degraded(): boolean {
    return this.fellBack;
  }

  // ------------------------------------------------------------- lifecycle

  /**
   * Load the player at `at`, stopping when it reaches `stopAt`.
   *
   * Replacing the element rather than reassigning `src` guarantees the previous
   * player stops, instead of leaving audio playing behind the new one.
   */
  load(videoId: string, at: number, stopAt: number): void {
    this.videoId = videoId;
    this.stopAt = stopAt;
    this.reset();

    const frame = document.createElement("iframe");
    frame.src = this.embedUrl(at);
    frame.title = "Slice preview";
    frame.allow = "autoplay; encrypted-media; picture-in-picture; fullscreen";
    frame.referrerPolicy = "strict-origin-when-cross-origin";
    frame.setAttribute("allowfullscreen", "");
    frame.addEventListener("load", () => this.shakeHands());

    this.stage.replaceChildren(frame);
    this.frame = frame;
    this.callbacks.onStatus(this.fellBack ? "degraded" : "connecting");

    // Start knocking immediately rather than only from the `load` handler: a
    // frame that never fires `load` would otherwise leave the player in limbo
    // instead of falling back.
    this.shakeHands();
  }

  close(): void {
    this.reset();
    this.frame = null;
    this.fellBack = false;
    this.stage.replaceChildren(
      Object.assign(document.createElement("p"), {
        className: "preview-placeholder",
        textContent: this.placeholder,
      }),
    );
    this.callbacks.onPlayhead(null);
  }

  /**
   * Stop playback without losing the loaded video — used when the tab is
   * switched away from, where audio continuing behind a hidden panel would be
   * the single most annoying bug in the app.
   *
   * Returns false when the player cannot be reached, in which case the caller
   * must close the preview instead: an unreachable player is not one to trust
   * with staying quiet.
   */
  pause(): boolean {
    if (!this.frame) return true;
    if (!this.ready) return false;
    this.command("pauseVideo");
    return true;
  }

  // ------------------------------------------------------------- commands

  /**
   * Move the player's cursor. Safe to call on every pointer event: calls are
   * coalesced to one seek per animation frame and rate-limited beyond that,
   * and the last position requested always lands.
   */
  seek(seconds: number): void {
    if (!this.frame || this.fellBack) return;
    this.queued = Math.max(0, seconds);
    this.scheduleFlush();
  }

  /** Move the stop point without touching the player — no reload involved. */
  setStop(seconds: number): void {
    this.stopAt = seconds;
  }

  /** Resume playback from wherever the cursor currently sits. */
  play(): void {
    this.command("playVideo");
  }

  /** Suspend the auto-stop while a handle is under the cursor. */
  beginScrub(): void {
    this.scrubbing = true;
  }

  endScrub(): void {
    this.scrubbing = false;
  }

  // ------------------------------------------------------------- internals

  /**
   * `end` is deliberately absent: it only accepts whole seconds and, more
   * importantly, freezes the stop point into the URL, so moving a handle would
   * need a reload. The slice end is enforced from the player's own reported
   * position instead — which is also more precise than the parameter.
   *
   * `origin` is likewise omitted. The widget validates it against the sender
   * when present, and a Tauri webview's origin (`tauri://localhost`,
   * `http://tauri.localhost`) is not one YouTube is prepared to see.
   */
  private embedUrl(at: number): string {
    const params = new URLSearchParams({
      start: String(Math.max(0, Math.floor(at))),
      autoplay: "1",
      rel: "0",
      modestbranding: "1",
      playsinline: "1",
      enablejsapi: "1",
    });

    // A degraded player cannot be stopped by us, so it gets the blunt
    // URL-level bound instead.
    if (this.fellBack) {
      params.set("end", String(Math.max(Math.floor(at) + 1, Math.ceil(this.stopAt))));
    }
    return `${EMBED_ORIGIN}/embed/${this.videoId}?${params.toString()}`;
  }

  private shakeHands(): void {
    if (this.fellBack) return;
    this.stopHandshake();
    this.attempts = 0;

    const knock = () => {
      if (this.ready) {
        this.stopHandshake();
        return;
      }
      if (this.attempts >= HANDSHAKE_ATTEMPTS) {
        this.stopHandshake();
        this.fellBack = true;
        this.callbacks.onStatus("degraded");
        return;
      }
      this.attempts += 1;
      this.post({ event: "listening", id: WIDGET_ID, channel: "widget" });
    };

    knock();
    this.handshakeTimer = window.setInterval(knock, HANDSHAKE_INTERVAL_MS);
  }

  private stopHandshake(): void {
    if (this.handshakeTimer !== null) {
      window.clearInterval(this.handshakeTimer);
      this.handshakeTimer = null;
    }
  }

  private readonly receive = (event: MessageEvent): void => {
    // Two independent checks: the origin proves who sent it, the source proves
    // it came from *our* frame rather than another embed on the page.
    if (event.origin !== EMBED_ORIGIN) return;
    if (!this.frame || event.source !== this.frame.contentWindow) return;
    if (typeof event.data !== "string") return;

    let message: unknown;
    try {
      message = JSON.parse(event.data);
    } catch {
      return;
    }
    if (typeof message !== "object" || message === null) return;

    // Any reply at all proves the widget is listening to us.
    if (!this.ready) {
      this.ready = true;
      this.stopHandshake();
      this.callbacks.onStatus("live");
      this.flush();
    }

    const { event: name, info } = message as {
      event?: string;
      info?: { currentTime?: number; playerState?: number };
    };
    if (name !== "infoDelivery" || !info) return;

    if (typeof info.playerState === "number") {
      this.playing = info.playerState === PLAYING;
    }
    if (typeof info.currentTime === "number") {
      this.track(info.currentTime);
    }
  };

  /** Follow the player's position and stop it at the end of the slice. */
  private track(seconds: number): void {
    this.callbacks.onPlayhead(seconds);

    // Suppressed while scrubbing, and briefly after any seek: dragging the end
    // handle parks the cursor exactly on the stop point, and pausing the moment
    // the user arrives there would fight them.
    const settled = performance.now() - this.lastSeekAt > SEEK_QUIET_MS;
    if (this.playing && settled && !this.scrubbing && seconds >= this.stopAt) {
      this.command("pauseVideo");
    }
  }

  private scheduleFlush(): void {
    if (this.frameRequest !== null) return;
    this.frameRequest = requestAnimationFrame(() => {
      this.frameRequest = null;
      this.flush();
    });
  }

  private flush(): void {
    if (this.queued === null || !this.ready) return;

    // Too soon to seek again: keep the target queued and try next frame, so the
    // position the user finally settles on is always the one that lands.
    if (performance.now() - this.lastSeekAt < SEEK_INTERVAL_MS) {
      this.scheduleFlush();
      return;
    }

    const target = this.queued;
    this.queued = null;
    this.lastSeekAt = performance.now();
    this.command("seekTo", [target, true]);
    this.callbacks.onPlayhead(target);
  }

  private command(func: string, args: unknown[] = []): void {
    if (!this.ready) return;
    this.post({ event: "command", func, args, id: WIDGET_ID, channel: "widget" });
  }

  private post(payload: Record<string, unknown>): void {
    this.frame?.contentWindow?.postMessage(JSON.stringify(payload), EMBED_ORIGIN);
  }

  /** Drop everything tied to the current frame, keeping `fellBack`. */
  private reset(): void {
    this.stopHandshake();
    if (this.frameRequest !== null) {
      cancelAnimationFrame(this.frameRequest);
      this.frameRequest = null;
    }
    this.ready = false;
    this.playing = false;
    this.scrubbing = false;
    this.queued = null;
    this.lastSeekAt = 0;
  }
}
