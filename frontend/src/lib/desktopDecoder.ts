/**
 * AV1 decode for the desktop stream, on the browser's own decoder.
 *
 * The daemon sends one rav1e temporal unit per chunk — a full frame's
 * worth of OBUs, with the sequence header prepended on every keyframe —
 * which is exactly the shape `EncodedVideoChunk` takes. WebCodecs'
 * `VideoDecoder` decodes AV1 natively on every browser flyco's users
 * have, so the stream costs no WASM download and no emulation tier.
 *
 * The decoder is strict about order: a `delta` chunk on a fresh decoder
 * is an error, so after a reset — a decode failure, a room-side `resync`,
 * a reconnect — chunks are dropped until the next `key` chunk, which the
 * daemon always sends when a watcher appears or the tail is cut.
 */

/** The codec string for 8-bit 4:2:0 AV1, Main profile, level 3.1. */
const AV1_CODEC = "av01.0.08M.08";

/** Nominal step between synthetic timestamps — decoders only need monotonic. */
const TIMESTAMP_STEP_US = 33_333;

/** Whether this browser can decode the desktop stream at all. */
export async function desktopDecodeSupported(): Promise<boolean> {
  if (typeof VideoDecoder === "undefined") {
    return false;
  }
  try {
    const support = await VideoDecoder.isConfigSupported({ codec: AV1_CODEC });
    return support.supported === true;
  } catch {
    return false;
  }
}

/**
 * A `VideoDecoder` behind the stream's rules: `key` chunks start it,
 * `delta` chunks continue it, and `reset()` parks it until the next key.
 */
export class DesktopDecoder {
  private decoder: VideoDecoder | null = null;
  /**
   * Whether the decoder has a keyframe behind it. A delta pushed before
   * one is dropped rather than decoded — a decoder that lost its tail
   * cannot span the gap.
   */
  private ready = false;
  private nextTimestamp = 0;
  private readonly onFrame: (frame: VideoFrame) => void;
  private readonly onError: (error: unknown) => void;

  constructor(options: {
    onFrame: (frame: VideoFrame) => void;
    onError: (error: unknown) => void;
  }) {
    this.onFrame = options.onFrame;
    this.onError = options.onError;
  }

  /** Starts the decoder. Idempotent — a second start is a reset. */
  start(): void {
    this.reset();
  }

  /** Feeds one temporal unit. Deltas before the first keyframe are dropped. */
  push(data: Uint8Array, keyframe: boolean): void {
    if (this.decoder === null) {
      this.start();
    }
    if (!this.ready) {
      if (!keyframe) {
        return;
      }
      this.ready = true;
    }
    const timestamp = this.nextTimestamp;
    this.nextTimestamp += TIMESTAMP_STEP_US;
    try {
      this.decoder?.decode(
        new EncodedVideoChunk({
          type: keyframe ? "key" : "delta",
          timestamp,
          data: data as BufferSource,
        }),
      );
    } catch (error) {
      this.onError(error);
      this.reset();
    }
  }

  /** Parks the decoder until the next keyframe. */
  reset(): void {
    this.ready = false;
    this.nextTimestamp = 0;
    if (this.decoder !== null && this.decoder.state !== "closed") {
      this.decoder.close();
    }
    this.decoder = new VideoDecoder({
      output: (frame) => this.onFrame(frame),
      error: (error) => {
        this.ready = false;
        this.onError(error);
      },
    });
    this.decoder.configure({
      codec: AV1_CODEC,
      optimizeForLatency: true,
    });
  }

  /** Closes the decoder for good. */
  dispose(): void {
    if (this.decoder !== null && this.decoder.state !== "closed") {
      this.decoder.close();
    }
    this.decoder = null;
  }
}
