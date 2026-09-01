import { describe, expect, it } from 'vitest';

import { parseWebTransportPacket } from './webtransport-media';

function packet(payload: number[]): Uint8Array {
  const bytes = new Uint8Array(40 + payload.length);
  const view = new DataView(bytes.buffer);
  view.setUint32(0, 0x47444d42, true);
  view.setUint8(4, 1);
  view.setUint8(6, 1);
  view.setUint8(7, 0xff);
  view.setUint32(12, 2, true);
  view.setBigUint64(16, 9n, true);
  view.setUint16(24, 0, true);
  view.setUint16(26, 1, true);
  view.setUint32(28, payload.length, true);
  view.setUint16(36, payload.length, true);
  bytes.set(payload, 40);
  return bytes;
}

describe('WebTransport datagram parser', () => {
  it('parses bounded fragment identity and payload', () => {
    const parsed = parseWebTransportPacket(packet([1, 2, 3]));
    expect(parsed?.header).toMatchObject({
      generation: 2,
      sequence: 9n,
      fragmentIndex: 0,
      fragmentCount: 1,
      dependency: 'reference',
    });
    expect(Array.from(parsed!.payload)).toEqual([1, 2, 3]);
  });

  it('rejects malformed magic and allocation bounds', () => {
    const malformed = packet([1]);
    malformed[0] = 0;
    expect(parseWebTransportPacket(malformed)).toBeNull();

    const tooMany = packet([1]);
    new DataView(tooMany.buffer).setUint16(26, 4097, true);
    expect(parseWebTransportPacket(tooMany)).toBeNull();
  });
});
