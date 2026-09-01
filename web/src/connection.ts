/**
 * WebSocket-only connection to the Beam server.
 *
 * Video (H.264 Annex B) and audio (Opus) frames arrive as binary WebSocket
 * messages with a 24-byte header. Input events and signaling messages are
 * sent/received as JSON text messages.
 *
 * Binary frame header format (24 bytes, little-endian):
 *   [0..4]   magic: 0x56414542 ("BEAV" in LE)
 *   [4]      version: 1
 *   [5]      flags: bit 0 = keyframe, bit 1 = audio
 *   [6..8]   width (u16 LE)
 *   [8..10]  height (u16 LE)
 *   [10..12] reserved (u16, always 0)
 *   [12..20] timestamp_us (u64 LE) -- microseconds since capture start
 *   [20..24] payload_length (u32 LE)
 *   [24..]   payload
 */

import type { StreamDescriptor } from './session';
import { WebTransportMediaReceiver } from './webtransport-media';

export const FRAME_HEADER_SIZE = 24;
export const FRAME_V2_HEADER_SIZE = 48;
export const FRAME_MAGIC = 0x56414542; // "BEAV" in little-endian

/** Parsed binary frame header */
export type DependencyClass = 'key' | 'reference' | 'disposable';

export interface FrameExtension {
  streamGeneration: number;
  frameSequence: bigint;
  dependency: DependencyClass;
  codec: 'h264' | 'hevc';
  temporalId?: number;
  encodeCompleteDeltaUs?: number;
  agentSendDeltaUs?: number;
}

export interface FrameHeader {
  version: number;
  headerSize: number;
  flags: number;
  width: number;
  height: number;
  timestampUs: bigint;
  payloadLength: number;
  extension?: FrameExtension;
}

/**
 * Parse a 24-byte binary frame header from an ArrayBuffer.
 * Returns null if the buffer is too short, has bad magic, or is truncated.
 */
export function parseFrameHeader(
  data: ArrayBuffer
): { header: FrameHeader; payload: Uint8Array } | null {
  if (data.byteLength < FRAME_HEADER_SIZE) {
    return null;
  }

  const view = new DataView(data);
  const magic = view.getUint32(0, true);
  if (magic !== FRAME_MAGIC) {
    return null;
  }

  const version = view.getUint8(4);
  if (version !== 1 && version !== 2) return null;
  const headerSize = version === 2 ? view.getUint16(10, true) : FRAME_HEADER_SIZE;
  if (headerSize < FRAME_HEADER_SIZE || headerSize > data.byteLength) return null;
  if (version === 2 && headerSize < FRAME_V2_HEADER_SIZE) return null;

  const flags = view.getUint8(5);
  const width = view.getUint16(6, true);
  const height = view.getUint16(8, true);
  const timestampUs = view.getBigUint64(12, true);
  const payloadLength = view.getUint32(20, true);

  const expectedSize = headerSize + payloadLength;
  if (data.byteLength < expectedSize) {
    return null;
  }

  let extension: FrameExtension | undefined;
  if (version === 2) {
    const dependencyByte = view.getUint8(28);
    const codecByte = view.getUint8(29);
    if (dependencyByte > 2 || codecByte > 1) return null;
    const timingFlags = view.getUint8(31);
    const temporalId = view.getUint8(30);
    extension = {
      streamGeneration: view.getUint32(24, true),
      frameSequence: view.getBigUint64(32, true),
      dependency: (['key', 'reference', 'disposable'] as const)[dependencyByte],
      codec: codecByte === 0 ? 'h264' : 'hevc',
      ...(temporalId === 0xff ? {} : { temporalId }),
      ...(timingFlags & 0x01 ? { encodeCompleteDeltaUs: view.getUint32(40, true) } : {}),
      ...(timingFlags & 0x02 ? { agentSendDeltaUs: view.getUint32(44, true) } : {}),
    };
  }

  const payload = new Uint8Array(data, headerSize, payloadLength);
  return {
    header: { version, headerSize, flags, width, height, timestampUs, payloadLength, extension },
    payload,
  };
}

/**
 * Input events sent over the WebSocket as JSON text.
 * Compact wire format matching the Rust InputEvent enum (serde tag = "t").
 */
export type InputEvent =
  | { t: 'k'; c: number; d: boolean }
  | { t: 'm'; x: number; y: number }
  | { t: 'rm'; dx: number; dy: number }
  | { t: 'b'; b: number; d: boolean }
  | { t: 's'; dx: number; dy: number }
  | { t: 'c'; text: string }
  | { t: 'cp'; text: string }
  | { t: 'r'; w: number; h: number }
  | { t: 'ri'; css_w: number; css_h: number; dpr: number; request_generation: number }
  | { t: 'rk'; generation: number; reason: string }
  | { t: 'cs'; id: number; t0_us: number }
  | { t: 'mt'; webtransport_active: boolean }
  | { t: 'l'; layout: string }
  | { t: 'q'; mode: string }
  | { t: 'vs'; visible: boolean }
  | { t: 'mp'; id: number; sent_ms: number }
  | ClientMetricsReport
  | { t: 'cur'; css: string }
  | { t: 'fs'; id: string; name: string; size: number }
  | { t: 'fc'; id: string; data: string }
  | { t: 'fd'; id: string }
  | { t: 'fdr'; path: string }
  | { t: 'fds'; id: string; name: string; size: number }
  | { t: 'fdc'; id: string; data: string }
  | { t: 'fdd'; id: string }
  | { t: 'fde'; id: string; error: string };

/** Signaling/control messages received as JSON text from the server */
type ServerMessage = { type: 'session_ready' } | { type: 'error'; message: string };

type VoidCallback = () => void;
type VideoFrameCallback = (
  flags: number,
  width: number,
  height: number,
  timestampUs: bigint,
  payload: Uint8Array
) => void;
type AudioFrameCallback = (timestampUs: bigint, payload: Uint8Array) => void;

export interface RendererQualitySnapshot {
  fps?: number;
  decodeMs?: number;
  videoFramesDecodedTotal: number;
  videoFramesDroppedTotal: number;
  audioFramesDecodedTotal: number;
  audioDropoutsTotal: number;
  audioBufferDelayMs?: number;
  videoFramesReceivedTotal?: number;
  videoFramesPresentedTotal?: number;
  decodeQueueSize?: number;
  oldestFrameAgeMs?: number;
  presentationSubmitMs?: number;
  workerEventLoopLagMs?: number;
  streamGeneration?: number;
}

type ClientMetricsReport = {
  t: 'cm';
  latency_ms?: number;
  jitter_ms?: number;
  fps?: number;
  decode_ms?: number;
  video_bytes_per_second: number;
  audio_bytes_per_second: number;
  video_frames_decoded_total: number;
  video_frames_dropped_total: number;
  audio_frames_decoded_total: number;
  audio_dropouts_total: number;
  audio_buffer_delay_ms?: number;
  video_frames_received_total: number;
  video_frames_presented_total: number;
  sequence_gaps_total: number;
  recovery_requests_total: number;
  decode_queue_size?: number;
  oldest_frame_age_ms?: number;
  presentation_submit_ms?: number;
  worker_event_loop_lag_ms?: number;
  stream_generation?: number;
};

const MAX_RECONNECT_DELAY_MS = 30_000;
const BASE_RECONNECT_DELAY_MS = 1_000;
const MAX_RECONNECT_ATTEMPTS = 10;

/**
 * Manages WebSocket connection to the Beam server.
 * Binary messages carry video/audio frames; text messages carry input events
 * and signaling.
 */
export class BeamConnection {
  private sessionId: string;
  private token: string;
  private ws: WebSocket | null = null;
  private reconnectAttempt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private intentionalDisconnect = false;
  private metricsInterval: ReturnType<typeof setInterval> | null = null;
  private metricsSnapshotProvider: (() => RendererQualitySnapshot | null) | null = null;
  private videoBytesThisSecond = 0;
  private audioBytesThisSecond = 0;
  private metricsPingId = 0;
  private pendingMetricPings = new Map<number, number>();
  private pendingClockSync = new Map<number, number>();
  private clockOffsetUs: number | null = null;
  private clockUncertaintyUs: number | null = null;
  private latencyMs: number | null = null;
  private jitterMs: number | null = null;
  private activeGeneration: number | null = null;
  private lastVideoSequence: bigint | null = null;
  private awaitingRecovery = false;
  private recoveryRequestsTotal = 0;
  private lastRecoveryRequestMs = Number.NEGATIVE_INFINITY;
  private sequenceGapsTotal = 0;
  private streamDescriptor: StreamDescriptor | null = null;
  private webTransportReceiver: WebTransportMediaReceiver | null = null;
  private webTransportActive = false;

  // Callbacks
  private videoFrameCallback: VideoFrameCallback | null = null;
  private audioFrameCallback: AudioFrameCallback | null = null;
  private connectedCallback: VoidCallback | null = null;
  private disconnectCallback: VoidCallback | null = null;
  private reconnectingCallback: ((attempt: number, maxAttempts: number) => void) | null = null;
  private reconnectFailedCallback: VoidCallback | null = null;
  private agentMessageCallback: ((msg: InputEvent) => void) | null = null;
  private replacedCallback: VoidCallback | null = null;
  private agentExitedCallback: VoidCallback | null = null;
  private streamDescriptorCallback: ((descriptor: StreamDescriptor) => void) | null = null;

  constructor(sessionId: string, token: string) {
    this.sessionId = sessionId;
    this.token = token;
  }

  /** Register callback for decoded video frames */
  onVideoFrame(callback: VideoFrameCallback): void {
    this.videoFrameCallback = callback;
  }

  /** Register callback for decoded audio frames */
  onAudioFrame(callback: AudioFrameCallback): void {
    this.audioFrameCallback = callback;
  }

  /** Register callback for when the WebSocket connection opens */
  onConnected(callback: VoidCallback): void {
    this.connectedCallback = callback;
  }

  /** Register callback for when the connection is lost */
  onDisconnect(callback: VoidCallback): void {
    this.disconnectCallback = callback;
  }

  /** Register callback for reconnection attempts */
  onReconnecting(callback: (attempt: number, maxAttempts: number) => void): void {
    this.reconnectingCallback = callback;
  }

  /** Register callback for when all reconnection attempts exhausted */
  onReconnectFailed(callback: VoidCallback): void {
    this.reconnectFailedCallback = callback;
  }

  /** Register callback for agent-to-browser text messages (cursor, file download, clipboard) */
  onAgentMessage(callback: (msg: InputEvent) => void): void {
    this.agentMessageCallback = callback;
  }

  /** Register callback for when this tab was replaced by another tab/window */
  onReplaced(callback: VoidCallback): void {
    this.replacedCallback = callback;
  }

  /** Register callback for when the agent process exited unexpectedly */
  onAgentExited(callback: VoidCallback): void {
    this.agentExitedCallback = callback;
  }

  onStreamDescriptor(callback: (descriptor: StreamDescriptor) => void): void {
    this.streamDescriptorCallback = callback;
    if (this.streamDescriptor) callback(this.streamDescriptor);
  }

  setInitialStreamDescriptor(descriptor: StreamDescriptor | undefined): void {
    if (!descriptor) return;
    this.streamDescriptor = descriptor;
    this.activeGeneration = descriptor.stream_generation;
    this.streamDescriptorCallback?.(descriptor);
  }

  /** Update the token (after refresh) so reconnections use the new one */
  updateToken(token: string): void {
    this.token = token;
  }

  /** Establish WebSocket connection */
  async connect(): Promise<void> {
    this.intentionalDisconnect = false;
    this.reconnectAttempt = 0;
    await this.establishConnection();
  }

  /** Cleanly tear down the connection */
  disconnect(): void {
    this.intentionalDisconnect = true;
    this.stopClientMetrics();
    this.cleanup();
  }

  /** Send an input event as JSON text over WebSocket */
  sendInput(event: InputEvent): void {
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(event));
    }
  }

  startClientMetrics(snapshotProvider: () => RendererQualitySnapshot | null): void {
    this.stopClientMetrics();
    this.metricsSnapshotProvider = snapshotProvider;
    this.sendMetricsPing();
    this.metricsInterval = setInterval(() => {
      this.sendMetricsPing();
      this.sendClientMetricsReport();
    }, 1000);
  }

  stopClientMetrics(): void {
    if (this.metricsInterval) {
      clearInterval(this.metricsInterval);
      this.metricsInterval = null;
    }
    this.metricsSnapshotProvider = null;
    this.pendingMetricPings.clear();
    this.pendingClockSync.clear();
    this.videoBytesThisSecond = 0;
    this.audioBytesThisSecond = 0;
    this.latencyMs = null;
    this.jitterMs = null;
  }

  private async establishConnection(): Promise<void> {
    this.cleanup();

    const wsProtocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const wsUrl = `${wsProtocol}//${location.host}/api/sessions/${this.sessionId}/ws?token=${encodeURIComponent(this.token)}`;

    this.ws = new WebSocket(wsUrl);
    this.ws.binaryType = 'arraybuffer';

    let wsOpened = false;

    this.ws.onopen = () => {
      wsOpened = true;
      this.reconnectAttempt = 0;
      this.sendMetricsPing();
      void this.startWebTransportIfNegotiated();
      this.connectedCallback?.();
    };

    this.ws.onmessage = (event: MessageEvent) => {
      if (event.data instanceof ArrayBuffer) {
        this.handleBinaryMessage(event.data, 'wss');
      } else if (typeof event.data === 'string') {
        this.handleTextMessage(event.data);
      }
    };

    this.ws.onclose = (event: CloseEvent) => {
      console.log(
        `WebSocket closed: code=${event.code} reason=${event.reason} clean=${event.wasClean} intentional=${this.intentionalDisconnect}`
      );
      setTimeout(() => {
        if (!this.intentionalDisconnect) {
          if (!wsOpened && event.code === 1006) {
            console.error('WebSocket rejected (likely auth failure), not retrying');
            this.reconnectFailedCallback?.();
            return;
          }
          this.scheduleReconnect();
        }
        this.disconnectCallback?.();
      }, 50);
    };

    this.ws.onerror = () => {
      // onclose fires after onerror; reconnect handled there
    };
  }

  private binaryMessageCount = 0;

  /** Parse a binary frame and dispatch to video/audio callback. */
  private handleBinaryMessage(data: ArrayBuffer, source: 'wss' | 'webtransport'): void {
    this.binaryMessageCount++;
    if (this.binaryMessageCount <= 3) {
      console.log(`[Beam] Binary message #${this.binaryMessageCount}: ${data.byteLength} bytes`);
    }

    const result = parseFrameHeader(data);
    if (!result) {
      console.warn('Invalid binary frame:', data.byteLength, 'bytes');
      return;
    }

    const { header, payload } = result;
    const isAudio = (header.flags & 0x02) !== 0;

    if (!isAudio && source === 'wss' && this.webTransportActive && (header.flags & 0x01) === 0) {
      return; // Datagram deltas; reliable WSS still carries recovery keyframes.
    }

    if (isAudio) {
      this.audioBytesThisSecond += data.byteLength;
      this.audioFrameCallback?.(header.timestampUs, payload);
    } else {
      this.videoBytesThisSecond += data.byteLength;
      if (!this.admitVideoHeader(header)) return;
      this.videoFrameCallback?.(
        header.flags,
        header.width,
        header.height,
        header.timestampUs,
        payload
      );
    }
  }

  private admitVideoHeader(header: FrameHeader): boolean {
    const ext = header.extension;
    const wireKeyframe = (header.flags & 0x01) !== 0;
    if (!ext) {
      if (wireKeyframe) this.awaitingRecovery = false;
      return !this.awaitingRecovery || wireKeyframe;
    }

    const isKey = ext.dependency === 'key' || wireKeyframe;
    if (this.activeGeneration === null || ext.streamGeneration > this.activeGeneration) {
      this.activeGeneration = ext.streamGeneration;
      this.lastVideoSequence = null;
      this.awaitingRecovery = true;
      if (this.streamDescriptor) {
        const sizing = this.streamDescriptor.sizing;
        this.streamDescriptor = {
          ...this.streamDescriptor,
          stream_generation: ext.streamGeneration,
          sizing: {
            ...sizing,
            encoded_width: header.width,
            encoded_height: header.height,
            effective_dpr_x: header.width / Math.max(1, sizing.css_width),
            effective_dpr_y: header.height / Math.max(1, sizing.css_height),
          },
        };
        this.streamDescriptorCallback?.(this.streamDescriptor);
      }
    } else if (ext.streamGeneration < this.activeGeneration) {
      return false;
    }

    const expected = this.lastVideoSequence === null ? null : this.lastVideoSequence + 1n;
    if (expected !== null && ext.frameSequence !== expected && !isKey) {
      this.sequenceGapsTotal++;
      this.awaitingRecovery = true;
      this.requestRecovery(ext.streamGeneration, 'browser_sequence_gap');
      this.lastVideoSequence = ext.frameSequence;
      return false;
    }
    this.lastVideoSequence = ext.frameSequence;

    if (this.awaitingRecovery && !isKey) return false;
    if (isKey) this.awaitingRecovery = false;
    return true;
  }

  private requestRecovery(generation: number, reason: string): void {
    if (this.ws?.readyState !== WebSocket.OPEN) return;
    const now = performance.now();
    if (now - this.lastRecoveryRequestMs < 250) return;
    this.lastRecoveryRequestMs = now;
    this.awaitingRecovery = true;
    this.recoveryRequestsTotal++;
    this.ws.send(JSON.stringify({ t: 'rk', generation, reason }));
  }

  private async startWebTransportIfNegotiated(): Promise<void> {
    if (
      this.streamDescriptor?.media_transport !== 'webtransport_datagram' ||
      !WebTransportMediaReceiver.supported() ||
      this.webTransportReceiver
    ) {
      return;
    }
    const receiver = new WebTransportMediaReceiver(
      this.sessionId,
      this.token,
      (frame) => this.handleBinaryMessage(frame, 'webtransport'),
      (generation, reason) => this.requestRecovery(generation, reason),
      (reason) => this.fallbackFromWebTransport(reason)
    );
    this.webTransportReceiver = receiver;
    try {
      await receiver.connect();
      if (this.webTransportReceiver !== receiver) return;
      this.webTransportActive = true;
      this.sendInput({ t: 'mt', webtransport_active: true });
      this.requestRecovery(this.activeGeneration ?? 0, 'webtransport_client_ready');
    } catch {
      if (this.webTransportReceiver === receiver) this.fallbackFromWebTransport('setup_failed');
    }
  }

  private fallbackFromWebTransport(reason: string): void {
    this.webTransportActive = false;
    this.sendInput({ t: 'mt', webtransport_active: false });
    this.webTransportReceiver?.close();
    this.webTransportReceiver = null;
    if (this.streamDescriptor?.media_transport === 'webtransport_datagram') {
      this.streamDescriptor = {
        ...this.streamDescriptor,
        media_transport: 'websocket',
        fallback_reasons: [...this.streamDescriptor.fallback_reasons, `webtransport_${reason}`],
      };
      this.streamDescriptorCallback?.(this.streamDescriptor);
    }
    this.requestRecovery(this.activeGeneration ?? 0, `webtransport_${reason}`);
  }

  /** Handle incoming JSON text messages (signaling + agent messages) */
  private handleTextMessage(data: string): void {
    let parsed: unknown;
    try {
      parsed = JSON.parse(data);
    } catch {
      console.warn('Failed to parse text message:', data);
      return;
    }

    if (typeof parsed !== 'object' || parsed === null) return;
    const msg = parsed as Record<string, unknown>;

    // Server signaling messages
    if (msg['type'] === 'clock_sync_reply') {
      this.handleClockSyncReply(msg);
      return;
    }

    if (msg['type'] === 'metrics_pong') {
      this.handleMetricsPong(msg);
      return;
    }

    if (msg['type'] === 'stream_descriptor' && typeof msg['descriptor'] === 'object') {
      const descriptor = msg['descriptor'] as StreamDescriptor;
      this.streamDescriptor = descriptor;
      this.activeGeneration = descriptor.stream_generation;
      this.lastVideoSequence = null;
      this.awaitingRecovery = true;
      this.streamDescriptorCallback?.(descriptor);
      return;
    }

    if (msg['type'] === 'error') {
      const serverMsg = msg as ServerMessage & { type: 'error' };
      if (serverMsg.message === 'replaced') {
        console.log('Session taken over by another tab');
        this.intentionalDisconnect = true;
        this.cleanup();
        this.replacedCallback?.();
        return;
      }
      if (serverMsg.message === 'agent_exited') {
        console.error('Agent process exited unexpectedly');
        this.intentionalDisconnect = true;
        this.cleanup();
        this.agentExitedCallback?.();
        return;
      }
      console.error('Server error:', serverMsg.message);
      return;
    }

    // Agent-to-browser messages (clipboard, cursor, file download events)
    // These have a "t" field matching the InputEvent discriminator
    if (msg['t']) {
      this.agentMessageCallback?.(msg as InputEvent);
    }
  }

  private sendMetricsPing(): void {
    if (this.ws?.readyState !== WebSocket.OPEN) return;
    const id = ++this.metricsPingId;
    const sentMs = performance.now();
    this.pendingMetricPings.set(id, sentMs);
    this.ws.send(JSON.stringify({ t: 'mp', id, sent_ms: sentMs }));
    const t0Us = Math.round(performance.now() * 1000);
    this.pendingClockSync.set(id, t0Us);
    this.ws.send(JSON.stringify({ t: 'cs', id, t0_us: t0Us }));

    if (this.pendingMetricPings.size > 32) {
      const oldest = this.pendingMetricPings.keys().next().value;
      if (oldest !== undefined) {
        this.pendingMetricPings.delete(oldest);
        this.pendingClockSync.delete(oldest);
      }
    }
  }

  private handleClockSyncReply(msg: Record<string, unknown>): void {
    const id = typeof msg['id'] === 'number' ? msg['id'] : null;
    const t1 = typeof msg['t1_us'] === 'number' ? msg['t1_us'] : null;
    const t2 = typeof msg['t2_us'] === 'number' ? msg['t2_us'] : null;
    if (id === null || t1 === null || t2 === null) return;
    const t0 = this.pendingClockSync.get(id);
    if (t0 === undefined || t2 < t1) return;
    this.pendingClockSync.delete(id);
    const t3 = Math.round(performance.now() * 1000);
    if (t3 < t0) return;
    const networkRtt = Math.max(0, t3 - t0 - (t2 - t1));
    this.clockOffsetUs = (t1 - t0 + (t2 - t3)) / 2;
    this.clockUncertaintyUs = Math.max(1, networkRtt / 2);
  }

  getClockEstimate(): { offsetUs: number; uncertaintyUs: number } | null {
    if (this.clockOffsetUs === null || this.clockUncertaintyUs === null) return null;
    return { offsetUs: this.clockOffsetUs, uncertaintyUs: this.clockUncertaintyUs };
  }

  private handleMetricsPong(msg: Record<string, unknown>): void {
    const id = typeof msg['id'] === 'number' ? msg['id'] : null;
    if (id === null) return;
    const sentMs = this.pendingMetricPings.get(id);
    if (sentMs === undefined) return;
    this.pendingMetricPings.delete(id);

    const rtt = Math.max(0, performance.now() - sentMs);
    if (Number.isFinite(rtt)) {
      if (this.latencyMs !== null) {
        this.jitterMs = Math.abs(rtt - this.latencyMs);
      }
      this.latencyMs = rtt;
    }
  }

  private sendClientMetricsReport(): void {
    if (this.ws?.readyState !== WebSocket.OPEN || !this.metricsSnapshotProvider) return;
    const snapshot = this.metricsSnapshotProvider();
    if (!snapshot) return;

    const report: ClientMetricsReport = {
      t: 'cm',
      video_bytes_per_second: this.videoBytesThisSecond,
      audio_bytes_per_second: this.audioBytesThisSecond,
      video_frames_decoded_total: snapshot.videoFramesDecodedTotal,
      video_frames_dropped_total: snapshot.videoFramesDroppedTotal,
      audio_frames_decoded_total: snapshot.audioFramesDecodedTotal,
      audio_dropouts_total: snapshot.audioDropoutsTotal,
      video_frames_received_total: snapshot.videoFramesReceivedTotal ?? 0,
      video_frames_presented_total: snapshot.videoFramesPresentedTotal ?? 0,
      sequence_gaps_total: this.sequenceGapsTotal,
      recovery_requests_total: this.recoveryRequestsTotal,
    };

    addFiniteMetric(report, 'latency_ms', this.latencyMs);
    addFiniteMetric(report, 'jitter_ms', this.jitterMs);
    addFiniteMetric(report, 'fps', snapshot.fps);
    addFiniteMetric(report, 'decode_ms', snapshot.decodeMs);
    addFiniteMetric(report, 'audio_buffer_delay_ms', snapshot.audioBufferDelayMs);
    addFiniteMetric(report, 'decode_queue_size', snapshot.decodeQueueSize);
    addFiniteMetric(report, 'oldest_frame_age_ms', snapshot.oldestFrameAgeMs);
    addFiniteMetric(report, 'presentation_submit_ms', snapshot.presentationSubmitMs);
    addFiniteMetric(report, 'worker_event_loop_lag_ms', snapshot.workerEventLoopLagMs);
    addFiniteMetric(report, 'stream_generation', snapshot.streamGeneration);

    this.videoBytesThisSecond = 0;
    this.audioBytesThisSecond = 0;
    this.ws.send(JSON.stringify(report));
  }

  private scheduleReconnect(): void {
    if (this.intentionalDisconnect || this.reconnectTimer) return;

    if (this.reconnectAttempt >= MAX_RECONNECT_ATTEMPTS) {
      console.error(`Max reconnect attempts (${MAX_RECONNECT_ATTEMPTS}) reached`);
      this.reconnectFailedCallback?.();
      return;
    }

    const baseDelay = Math.min(
      BASE_RECONNECT_DELAY_MS * Math.pow(2, this.reconnectAttempt),
      MAX_RECONNECT_DELAY_MS
    );
    const jitter = Math.random() * baseDelay * 0.3;
    const delay = Math.round(baseDelay + jitter);

    this.reconnectAttempt++;

    console.log(
      `Reconnecting in ${delay}ms (attempt ${this.reconnectAttempt}/${MAX_RECONNECT_ATTEMPTS})...`
    );
    this.reconnectingCallback?.(this.reconnectAttempt, MAX_RECONNECT_ATTEMPTS);

    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.establishConnection();
    }, delay);
  }

  private cleanup(): void {
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }

    this.webTransportActive = false;
    this.webTransportReceiver?.close();
    this.webTransportReceiver = null;

    if (this.ws) {
      this.ws.close();
      this.ws = null;
    }
  }
}

function addFiniteMetric(
  report: Record<string, unknown>,
  key: string,
  value: number | null | undefined
): void {
  if (typeof value === 'number' && Number.isFinite(value)) {
    report[key] = value;
  }
}
