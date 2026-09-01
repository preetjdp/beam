import type { RendererQualitySnapshot } from './connection';
import { extractCodecFromAnnexB } from './h264';

/**
 * WebCodecs-based video/audio renderer for the Beam remote desktop client.
 * Video frames arrive as H.264 Annex B payloads over WebSocket binary messages,
 * are decoded via VideoDecoder, and drawn to a <canvas> via drawImage(VideoFrame).
 *
 * Audio frames arrive as Opus payloads, decoded via AudioDecoder, and played
 * through an AudioContext.
 */
export class WebCodecsRenderer {
  private decoder: VideoDecoder | null = null;
  private audioDecoder: AudioDecoder | null = null;
  private audioContext: AudioContext | null = null;
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private containerElement: HTMLElement;
  private currentWidth = 0;
  private currentHeight = 0;
  private framesDecoded = 0;
  private prevFrameCount = 0;
  private currentFps = 0;
  private fpsInterval: ReturnType<typeof setInterval> | null = null;
  private audioMuted = true;
  private nextAudioPlayTime = 0;
  private muteChangeCallback: ((muted: boolean) => void) | null = null;
  private fpsCallback: ((fps: number, decodeMs: number) => void) | null = null;
  private firstFrameCallback: (() => void) | null = null;
  private firstFrameFired = false;
  private lastFeedTimeMs = 0;
  private decodeTimeMs = 0;
  private needsKeyframe = true;
  private videoFrameCount = 0;
  private audioFrameCount = 0;
  private videoFramesDropped = 0;
  private audioFramesDecoded = 0;
  private audioDropouts = 0;
  private audioBufferDelayMs = 0;
  private videoFramesReceived = 0;
  private videoFramesPresented = 0;
  private pendingDecodeFeeds = new Map<number, number>();
  private oldestFrameAgeMs = 0;
  private presentationSubmitMs = 0;
  private streamGeneration = 0;
  private recoveryNeededCallback: ((generation: number, reason: string) => void) | null = null;
  private recoveryRequestOutstanding = false;
  private readonly decodeHighWatermark = 3;
  private readonly maxFrameAgeMs = 150;

  constructor(canvas: HTMLCanvasElement, containerElement: HTMLElement) {
    this.canvas = canvas;
    this.containerElement = containerElement;
    const ctx = canvas.getContext('2d', { alpha: false, desynchronized: true });
    if (!ctx) throw new Error('Failed to get 2d context from canvas');
    this.ctx = ctx;

    // Auto-unmute on first desktop click only if user has never set a preference.
    // Returning users get their saved preference restored by main.ts instead.
    if (localStorage.getItem('beam_audio_muted') === null) {
      this.containerElement.addEventListener(
        'click',
        () => {
          if (this.audioMuted) {
            this.setAudioMuted(false);
          }
        },
        { once: true }
      );
    }
  }

  setStreamGeneration(generation: number): void {
    if (generation === this.streamGeneration) return;
    this.streamGeneration = generation;
    this.pendingDecodeFeeds.clear();
    this.needsKeyframe = true;
    this.recoveryRequestOutstanding = false;
  }

  onRecoveryNeeded(callback: (generation: number, reason: string) => void): void {
    this.recoveryNeededCallback = callback;
  }

  /** Register callback for the first decoded video frame */
  onFirstFrame(callback: () => void): void {
    this.firstFrameCallback = callback;
  }

  /** Register callback for mute state changes */
  onMuteChange(callback: (muted: boolean) => void): void {
    this.muteChangeCallback = callback;
  }

  /** Returns true if audio is currently muted */
  isMuted(): boolean {
    return this.audioMuted;
  }

  /** Toggle audio mute state. Returns the new muted state. */
  toggleMute(): boolean {
    this.setAudioMuted(!this.audioMuted);
    return this.audioMuted;
  }

  /** Set audio mute state directly */
  setAudioMuted(muted: boolean): void {
    this.audioMuted = muted;
    if (!muted && !this.audioContext) {
      this.audioContext = new AudioContext({ sampleRate: 48000 });
    }
    if (this.audioContext) {
      if (muted) {
        this.audioContext.suspend();
      } else {
        this.audioContext.resume();
      }
    }
    this.muteChangeCallback?.(muted);
  }

  /** Returns true if we have received at least one frame */
  hasStream(): boolean {
    return this.firstFrameFired;
  }

  /** Get the canvas element (for screenshot capture) */
  getCanvas(): HTMLCanvasElement {
    return this.canvas;
  }

  /** Get current video width */
  getVideoWidth(): number {
    return this.currentWidth;
  }

  /** Get current video height */
  getVideoHeight(): number {
    return this.currentHeight;
  }

  /** Configure or reconfigure the video decoder for the given resolution */
  private configureDecoder(width: number, height: number, codec?: string): void {
    const codecStr = codec ?? 'avc1.4d0033';
    console.log(`[Beam] configureDecoder: ${width}x${height} codec=${codecStr}`);
    if (this.decoder) {
      try {
        this.decoder.close();
      } catch {
        /* already closed */
      }
      this.decoder = null;
    }

    this.currentWidth = width;
    this.currentHeight = height;
    this.canvas.width = width;
    this.canvas.height = height;

    this.decoder = new VideoDecoder({
      output: (frame: VideoFrame) => {
        const outputAt = performance.now();
        const feedAt = this.pendingDecodeFeeds.get(frame.timestamp);
        this.pendingDecodeFeeds.delete(frame.timestamp);
        if (feedAt !== undefined) this.decodeTimeMs = outputAt - feedAt;
        try {
          this.ctx.drawImage(frame, 0, 0);
          this.presentationSubmitMs = performance.now() - outputAt;
          this.framesDecoded++;
          this.videoFramesPresented++;
        } finally {
          frame.close();
        }
        this.updateQueueAge();

        if (!this.firstFrameFired) {
          console.log(
            `[Beam] First video frame decoded: ${frame.displayWidth}x${frame.displayHeight}`
          );
          this.firstFrameFired = true;
          this.firstFrameCallback?.();
        }
      },
      error: (err: DOMException) => {
        console.error('VideoDecoder error:', err);
      },
    });

    this.decoder.configure({
      codec: codecStr,
      hardwareAcceleration: 'prefer-hardware',
      optimizeForLatency: true,
    });
    this.needsKeyframe = true;
    this.pendingDecodeFeeds.clear();

    this.startFpsCounter();
  }

  /** Feed a video frame from the binary WebSocket message */
  feedVideoFrame(
    flags: number,
    width: number,
    height: number,
    timestampUs: bigint,
    payload: Uint8Array
  ): void {
    this.videoFrameCount++;
    this.videoFramesReceived++;
    if (this.videoFrameCount <= 5) {
      const isKf = (flags & 0x01) !== 0;
      console.log(
        `[Beam] feedVideoFrame #${this.videoFrameCount}: ${width}x${height} flags=0x${flags.toString(16)} keyframe=${isKf} payload=${payload.byteLength} decoderState=${this.decoder?.state ?? 'null'}`
      );
    }

    const isKeyframe = (flags & 0x01) !== 0;

    // Current H.264 IPPP frames are all references. Once queued work becomes
    // stale we reset the chain rather than dropping one P-frame and decoding
    // dependants into corruption.
    if (this.decoder && !isKeyframe) {
      this.updateQueueAge();
      if (
        this.decoder.decodeQueueSize > this.decodeHighWatermark ||
        this.oldestFrameAgeMs > this.maxFrameAgeMs
      ) {
        this.enterRecovery('browser_decode_pressure');
        this.videoFramesDropped++;
        return;
      }
    }

    // Reconfigure decoder if resolution changed
    if (width !== this.currentWidth || height !== this.currentHeight) {
      const codec = isKeyframe ? (extractCodecFromAnnexB(payload) ?? undefined) : undefined;
      this.configureDecoder(width, height, codec);
    }

    // Recover from closed decoder (decode error) on next keyframe
    if (this.decoder?.state === 'closed' && isKeyframe) {
      console.log('[Beam] Decoder closed, reconfiguring on keyframe');
      const codec = extractCodecFromAnnexB(payload) ?? undefined;
      this.configureDecoder(width, height, codec);
    }

    if (!this.decoder || this.decoder.state === 'closed') {
      this.videoFramesDropped++;
      return;
    }

    // If decoder not yet configured, skip
    if (this.decoder.state !== 'configured') {
      this.videoFramesDropped++;
      return;
    }

    // After configure() or flush(), decoder requires a keyframe first
    if (this.needsKeyframe && !isKeyframe) {
      this.videoFramesDropped++;
      return;
    }
    if (isKeyframe) {
      this.needsKeyframe = false;
      this.recoveryRequestOutstanding = false;
    }

    const chunk = new EncodedVideoChunk({
      type: isKeyframe ? 'key' : 'delta',
      timestamp: Number(timestampUs),
      data: payload,
    });

    try {
      this.lastFeedTimeMs = performance.now();
      this.pendingDecodeFeeds.set(Number(timestampUs), this.lastFeedTimeMs);
      this.decoder.decode(chunk);
      this.updateQueueAge();
    } catch (err) {
      this.videoFramesDropped++;
      console.error('VideoDecoder.decode() error:', err);
    }
  }

  private updateQueueAge(): void {
    const now = performance.now();
    let oldest = now;
    for (const queuedAt of this.pendingDecodeFeeds.values()) oldest = Math.min(oldest, queuedAt);
    this.oldestFrameAgeMs = this.pendingDecodeFeeds.size > 0 ? now - oldest : 0;
  }

  private enterRecovery(reason: string): void {
    if (this.decoder?.state === 'configured') {
      try {
        this.decoder.reset();
      } catch {
        // A concurrent decoder error may already have closed it.
      }
    }
    this.pendingDecodeFeeds.clear();
    this.needsKeyframe = true;
    if (!this.recoveryRequestOutstanding) {
      this.recoveryRequestOutstanding = true;
      this.recoveryNeededCallback?.(this.streamGeneration, reason);
    }
  }

  /** Feed an audio frame from the binary WebSocket message */
  feedAudioFrame(timestampUs: bigint, payload: Uint8Array): void {
    if (this.audioMuted || !this.audioContext) return;

    this.audioFrameCount++;
    if (this.audioFrameCount === 1) {
      console.log('Audio: first frame received', {
        payloadSize: payload.byteLength,
        audioContextState: this.audioContext.state,
      });
    }

    if (!this.audioDecoder) {
      this.nextAudioPlayTime = 0;
      let firstDecodeLogged = false;
      this.audioDecoder = new AudioDecoder({
        output: (audioData: AudioData) => {
          if (!firstDecodeLogged) {
            console.log('Audio: first decode successful', {
              frames: audioData.numberOfFrames,
              channels: audioData.numberOfChannels,
              sampleRate: audioData.sampleRate,
            });
            firstDecodeLogged = true;
          }
          // Play audio via AudioContext
          if (this.audioContext && this.audioContext.state === 'running') {
            const numFrames = audioData.numberOfFrames;
            const numChannels = audioData.numberOfChannels;
            const sampleRate = audioData.sampleRate;
            const buffer = this.audioContext.createBuffer(numChannels, numFrames, sampleRate);

            for (let ch = 0; ch < numChannels; ch++) {
              const channelData = buffer.getChannelData(ch);
              audioData.copyTo(channelData, { planeIndex: ch, format: 'f32-planar' });
            }

            const source = this.audioContext.createBufferSource();
            source.buffer = buffer;
            source.connect(this.audioContext.destination);

            const now = this.audioContext.currentTime;
            // Snap forward if we've fallen behind (network stall, tab resume)
            if (this.nextAudioPlayTime < now) {
              if (this.nextAudioPlayTime > 0) {
                this.audioDropouts++;
              }
              this.nextAudioPlayTime = now;
            }
            source.start(this.nextAudioPlayTime);
            this.nextAudioPlayTime += buffer.duration;
            this.audioBufferDelayMs = Math.max(0, (this.nextAudioPlayTime - now) * 1000);
          }
          this.audioFramesDecoded++;
          audioData.close();
        },
        error: (err: DOMException) => {
          console.error('AudioDecoder error:', err);
        },
      });

      this.audioDecoder.configure({
        codec: 'opus',
        sampleRate: 48000,
        numberOfChannels: 2,
      });
    }

    if (this.audioDecoder.state !== 'configured') return;

    const chunk = new EncodedAudioChunk({
      type: 'key', // Opus frames are always independently decodable
      timestamp: Number(timestampUs),
      data: payload,
    });

    try {
      this.audioDecoder.decode(chunk);
    } catch (err) {
      this.audioDropouts++;
      console.error('AudioDecoder.decode() error:', err);
    }
  }

  getQualitySnapshot(): RendererQualitySnapshot {
    return {
      fps: this.currentFps,
      decodeMs: this.decodeTimeMs,
      videoFramesDecodedTotal: this.framesDecoded,
      videoFramesDroppedTotal: this.videoFramesDropped,
      audioFramesDecodedTotal: this.audioFramesDecoded,
      audioDropoutsTotal: this.audioDropouts,
      audioBufferDelayMs: this.audioBufferDelayMs,
      videoFramesReceivedTotal: this.videoFramesReceived,
      videoFramesPresentedTotal: this.videoFramesPresented,
      decodeQueueSize: this.decoder?.decodeQueueSize ?? 0,
      oldestFrameAgeMs: this.oldestFrameAgeMs,
      presentationSubmitMs: this.presentationSubmitMs,
      streamGeneration: this.streamGeneration,
    };
  }

  /** Get current FPS value */
  getFps(): number {
    return this.currentFps;
  }

  /** Set callback to receive FPS and decode time updates */
  onFpsUpdate(callback: (fps: number, decodeMs: number) => void): void {
    this.fpsCallback = callback;
  }

  /** Enter fullscreen mode */
  enterFullscreen(): void {
    this.containerElement.requestFullscreen?.().catch((err) => {
      console.warn('Fullscreen request failed:', err);
    });
  }

  /** Exit fullscreen mode */
  exitFullscreen(): void {
    if (document.fullscreenElement) {
      document.exitFullscreen?.();
    }
  }

  /** Clean up resources */
  destroy(): void {
    this.stopFpsCounter();
    if (this.decoder) {
      try {
        this.decoder.close();
      } catch {
        /* already closed */
      }
      this.decoder = null;
    }
    if (this.audioDecoder) {
      try {
        this.audioDecoder.close();
      } catch {
        /* already closed */
      }
      this.audioDecoder = null;
    }
    this.nextAudioPlayTime = 0;
    if (this.audioContext) {
      this.audioContext.close();
      this.audioContext = null;
    }
    this.firstFrameFired = false;
    this.currentWidth = 0;
    this.currentHeight = 0;
    this.videoFramesDropped = 0;
    this.audioFramesDecoded = 0;
    this.audioDropouts = 0;
    this.audioBufferDelayMs = 0;
    this.videoFramesReceived = 0;
    this.videoFramesPresented = 0;
    this.pendingDecodeFeeds.clear();
    this.oldestFrameAgeMs = 0;
    this.presentationSubmitMs = 0;
    this.recoveryRequestOutstanding = false;
  }

  private startFpsCounter(): void {
    this.stopFpsCounter();
    this.prevFrameCount = this.framesDecoded;

    this.fpsInterval = setInterval(() => {
      const decoded = this.framesDecoded - this.prevFrameCount;
      this.currentFps = decoded;
      this.prevFrameCount = this.framesDecoded;
      this.fpsCallback?.(this.currentFps, this.decodeTimeMs);
    }, 1000);
  }

  private stopFpsCounter(): void {
    if (this.fpsInterval) {
      clearInterval(this.fpsInterval);
      this.fpsInterval = null;
    }
  }
}
