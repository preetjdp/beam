const DATAGRAM_MAGIC = 0x47444d42; // bytes "BMDG" read little-endian
const DATAGRAM_HEADER_SIZE = 40;
const MAX_FRAMES_IN_FLIGHT = 8;
const MAX_REASSEMBLY_BYTES = 32 * 1024 * 1024;
const FRAME_DEADLINE_MS = 150;

export interface PacketHeader {
  generation: number;
  sequence: bigint;
  fragmentIndex: number;
  fragmentCount: number;
  totalLength: number;
  payloadLength: number;
  dependency: 'key' | 'reference' | 'disposable';
}

interface PartialFrame {
  generation: number;
  sequence: bigint;
  dependency: PacketHeader['dependency'];
  totalLength: number;
  deadline: number;
  fragments: Array<Uint8Array | null>;
  bytes: number;
}

interface WebTransportReader {
  read(): Promise<{ done: boolean; value?: Uint8Array }>;
  cancel(): Promise<void>;
  releaseLock(): void;
}

interface BrowserWebTransport {
  ready: Promise<void>;
  closed: Promise<unknown>;
  close(options?: { closeCode?: number; reason?: string }): void;
  datagrams: { readable: { getReader(): WebTransportReader } };
}

type WebTransportConstructor = new (url: string) => BrowserWebTransport;

export function parseWebTransportPacket(
  data: Uint8Array
): { header: PacketHeader; payload: Uint8Array } | null {
  if (data.byteLength < DATAGRAM_HEADER_SIZE) return null;
  const view = new DataView(data.buffer, data.byteOffset, data.byteLength);
  if (view.getUint32(0, true) !== DATAGRAM_MAGIC || view.getUint8(4) !== 1) return null;
  const dependencyByte = view.getUint8(6);
  if (dependencyByte > 2) return null;
  const fragmentIndex = view.getUint16(24, true);
  const fragmentCount = view.getUint16(26, true);
  const totalLength = view.getUint32(28, true);
  const payloadLength = view.getUint16(36, true);
  if (
    fragmentCount === 0 ||
    fragmentCount > 4096 ||
    fragmentIndex >= fragmentCount ||
    totalLength > 16 * 1024 * 1024 ||
    DATAGRAM_HEADER_SIZE + payloadLength > data.byteLength
  ) {
    return null;
  }
  return {
    header: {
      generation: view.getUint32(12, true),
      sequence: view.getBigUint64(16, true),
      fragmentIndex,
      fragmentCount,
      totalLength,
      payloadLength,
      dependency: (['key', 'reference', 'disposable'] as const)[dependencyByte],
    },
    payload: data.slice(DATAGRAM_HEADER_SIZE, DATAGRAM_HEADER_SIZE + payloadLength),
  };
}

export class WebTransportMediaReceiver {
  private transport: BrowserWebTransport | null = null;
  private reader: WebTransportReader | null = null;
  private frames = new Map<string, PartialFrame>();
  private reassemblyBytes = 0;
  private latestGeneration = 0;
  private lastCompletedSequence: bigint | null = null;
  private gapReportedAtSequence: bigint | null = null;
  private stopped = false;

  constructor(
    private readonly sessionId: string,
    private readonly token: string,
    private readonly onFrame: (frame: ArrayBuffer) => void,
    private readonly onRecovery: (generation: number, reason: string) => void,
    private readonly onFailure: (reason: string) => void
  ) {}

  static supported(): boolean {
    return (
      typeof (globalThis as typeof globalThis & { WebTransport?: unknown }).WebTransport !==
      'undefined'
    );
  }

  async connect(): Promise<void> {
    const Constructor = (
      globalThis as typeof globalThis & { WebTransport?: WebTransportConstructor }
    ).WebTransport;
    if (!Constructor) throw new Error('WebTransport unavailable');
    const url = `${location.origin}/webtransport/${this.sessionId}?token=${encodeURIComponent(this.token)}`;
    this.transport = new Constructor(url);
    await this.transport.ready;
    if (this.stopped) return;
    this.reader = this.transport.datagrams.readable.getReader();
    void this.transport.closed.then(
      () => {
        if (!this.stopped) this.onFailure('webtransport_closed');
      },
      () => {
        if (!this.stopped) this.onFailure('webtransport_failed');
      }
    );
    void this.readLoop();
  }

  close(): void {
    this.stopped = true;
    void this.reader?.cancel().catch(() => {});
    this.reader?.releaseLock();
    this.reader = null;
    this.transport?.close({ closeCode: 0, reason: 'client shutdown' });
    this.transport = null;
    this.frames.clear();
    this.reassemblyBytes = 0;
  }

  private async readLoop(): Promise<void> {
    try {
      while (!this.stopped && this.reader) {
        const { done, value } = await this.reader.read();
        if (done) break;
        if (value) this.acceptDatagram(value);
      }
    } catch {
      if (!this.stopped) this.onFailure('webtransport_read_error');
    }
  }

  private acceptDatagram(data: Uint8Array): void {
    const parsed = parseWebTransportPacket(data);
    if (!parsed) return;
    const { header, payload } = parsed;
    this.expireFrames(performance.now());

    if (header.generation < this.latestGeneration) return;
    if (header.generation > this.latestGeneration) {
      this.frames.clear();
      this.reassemblyBytes = 0;
      this.latestGeneration = header.generation;
      this.lastCompletedSequence = null;
      this.gapReportedAtSequence = null;
    }
    if (
      this.lastCompletedSequence !== null &&
      header.sequence > this.lastCompletedSequence + 1n &&
      header.dependency !== 'disposable' &&
      this.gapReportedAtSequence !== header.sequence
    ) {
      this.gapReportedAtSequence = header.sequence;
      this.onRecovery(header.generation, 'webtransport_sequence_gap');
    }

    const key = `${header.generation}:${header.sequence}`;
    let partial = this.frames.get(key);
    if (!partial) {
      if (
        this.frames.size >= MAX_FRAMES_IN_FLIGHT ||
        this.reassemblyBytes + payload.byteLength > MAX_REASSEMBLY_BYTES
      ) {
        this.onRecovery(header.generation, 'webtransport_reassembly_bound');
        return;
      }
      partial = {
        generation: header.generation,
        sequence: header.sequence,
        dependency: header.dependency,
        totalLength: header.totalLength,
        deadline: performance.now() + FRAME_DEADLINE_MS,
        fragments: Array.from({ length: header.fragmentCount }, () => null),
        bytes: 0,
      };
      this.frames.set(key, partial);
    }
    if (
      partial.fragments.length !== header.fragmentCount ||
      partial.totalLength !== header.totalLength
    ) {
      this.dropPartial(key, partial, true);
      return;
    }
    if (!partial.fragments[header.fragmentIndex]) {
      partial.fragments[header.fragmentIndex] = payload;
      partial.bytes += payload.byteLength;
      this.reassemblyBytes += payload.byteLength;
    }
    if (partial.fragments.some((fragment) => fragment === null)) return;

    const complete = new Uint8Array(partial.totalLength);
    let offset = 0;
    for (const fragment of partial.fragments) {
      if (!fragment || offset + fragment.byteLength > complete.byteLength) {
        this.dropPartial(key, partial, true);
        return;
      }
      complete.set(fragment, offset);
      offset += fragment.byteLength;
    }
    this.dropPartial(key, partial, false);
    if (offset !== complete.byteLength) return;
    this.lastCompletedSequence = header.sequence;
    if (header.dependency === 'key') this.gapReportedAtSequence = null;
    this.onFrame(complete.buffer);
  }

  private expireFrames(now: number): void {
    for (const [key, partial] of this.frames) {
      if (partial.deadline <= now) this.dropPartial(key, partial, true);
    }
  }

  private dropPartial(key: string, partial: PartialFrame, recover: boolean): void {
    this.frames.delete(key);
    this.reassemblyBytes = Math.max(0, this.reassemblyBytes - partial.bytes);
    if (recover && partial.dependency !== 'disposable') {
      this.onRecovery(partial.generation, 'webtransport_incomplete_frame');
    }
  }
}
