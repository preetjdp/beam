export interface CodecCapability {
  codec: 'h264' | 'hevc';
  codec_string: string;
  supported: boolean;
  hardware_acceleration: boolean;
}

export interface ClientCapabilities {
  protocol_versions: number[];
  frame_header_versions: number[];
  codecs: CodecCapability[];
  webcodecs_worker: boolean;
  offscreen_canvas: boolean;
  webtransport: boolean;
  webtransport_datagrams: boolean;
  max_decode_width?: number;
  max_decode_height?: number;
  max_decode_pixels?: number;
}

async function probeCodec(
  codec: 'h264' | 'hevc',
  codecString: string,
  width: number,
  height: number
): Promise<CodecCapability> {
  if (typeof VideoDecoder === 'undefined' || typeof VideoDecoder.isConfigSupported !== 'function') {
    return { codec, codec_string: codecString, supported: false, hardware_acceleration: false };
  }
  try {
    const result = await VideoDecoder.isConfigSupported({
      codec: codecString,
      codedWidth: width,
      codedHeight: height,
      hardwareAcceleration: 'prefer-hardware',
      optimizeForLatency: true,
    });
    return {
      codec,
      codec_string: codecString,
      supported: result.supported === true,
      // WebCodecs does not expose whether the accepted configuration will
      // actually use hardware. This means "hardware preferred", not claimed.
      hardware_acceleration: result.supported === true,
    };
  } catch {
    return { codec, codec_string: codecString, supported: false, hardware_acceleration: false };
  }
}

function supportsWorkerWebCodecs(): boolean {
  // Dedicated workers expose VideoDecoder on their own global. There is no
  // synchronous cross-realm probe, so gate on Worker plus the browser's
  // advertised WebCodecs surface and verify again inside a future worker.
  return typeof Worker !== 'undefined' && typeof VideoDecoder !== 'undefined';
}

export async function collectClientCapabilities(
  cssWidth: number,
  cssHeight: number,
  dpr: number
): Promise<ClientCapabilities> {
  const width = Math.max(320, Math.min(7680, Math.floor(cssWidth * dpr) & ~1));
  const height = Math.max(240, Math.min(4320, Math.floor(cssHeight * dpr) & ~1));
  const codecCandidates = await Promise.all([
    probeCodec('h264', 'avc1.4d0033', width, height),
    probeCodec('h264', 'avc1.640033', width, height),
    probeCodec('hevc', 'hvc1.1.6.L120.B0', width, height),
    probeCodec('hevc', 'hev1.1.6.L120.B0', width, height),
  ]);

  const webtransport =
    typeof (globalThis as typeof globalThis & { WebTransport?: unknown }).WebTransport !==
    'undefined';
  return {
    protocol_versions: [1],
    frame_header_versions: [1, 2],
    codecs: codecCandidates,
    webcodecs_worker: supportsWorkerWebCodecs(),
    offscreen_canvas:
      typeof OffscreenCanvas !== 'undefined' &&
      typeof HTMLCanvasElement !== 'undefined' &&
      'transferControlToOffscreen' in HTMLCanvasElement.prototype,
    webtransport,
    webtransport_datagrams: webtransport,
    max_decode_width: 7680,
    max_decode_height: 4320,
    max_decode_pixels: 33_177_600,
  };
}
