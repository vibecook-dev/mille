// Semantic message ⇄ frame bytes (SPEC §11.3–§11.5).
//
// A Mille message is a plain JSON-ish object that may carry binary views
// (bincode mirrors, viewport patches, packed child lists). MessagePort
// structured-clones those for free; a byte stream cannot, and JSON-encoding
// them would inflate a megabyte of entries into several.
//
// So the encoder walks the message, swaps each binary view for a
// `{$mille:'bin',i}` placeholder, and appends the referenced bytes verbatim
// after the metadata. The decoder reverses it. JSON pays only for structure;
// payloads stay raw.

import {
  FrameProtocolError,
  HEADER_BYTES,
  KNOWN_FLAGS,
  WIRE_MAGIC,
  WIRE_MAJOR,
  WIRE_MINOR,
  type FramedStreamLimits,
} from './limits.js';

/** Marker written in place of a binary view. */
export interface BinaryPlaceholder {
  readonly $mille: 'bin';
  readonly i: number;
}

function isBinaryPlaceholder(value: unknown): value is BinaryPlaceholder {
  if (typeof value !== 'object' || value === null) return false;
  const v = value as Partial<BinaryPlaceholder>;
  return v.$mille === 'bin' && typeof v.i === 'number';
}

/**
 * Narrow a value to the exact byte range it refers to.
 *
 * SPEC §11.5: a typed array is frequently a *view* onto a larger buffer —
 * `encodeClientEntries` hands back subarrays. Serializing `.buffer` would
 * ship the whole backing allocation and silently corrupt the payload on
 * decode. Always honour byteOffset/byteLength.
 */
function asBytes(value: unknown): Uint8Array | null {
  if (value instanceof Uint8Array) {
    return value;
  }
  if (ArrayBuffer.isView(value)) {
    const view = value as ArrayBufferView;
    return new Uint8Array(view.buffer, view.byteOffset, view.byteLength);
  }
  if (value instanceof ArrayBuffer) {
    return new Uint8Array(value);
  }
  return null;
}

function isSharedBuffer(value: unknown): boolean {
  // SharedArrayBuffer is absent in some runtimes; guard the reference.
  return (
    typeof SharedArrayBuffer !== 'undefined' &&
    (value instanceof SharedArrayBuffer ||
      (ArrayBuffer.isView(value) && (value as ArrayBufferView).buffer instanceof SharedArrayBuffer))
  );
}

/**
 * Only plain objects and arrays are structural. Anything with a exotic
 * prototype (Map, Set, Date, class instances) would silently lose its
 * identity through JSON, so refuse it rather than ship something the peer
 * will misread.
 */
function isPlainObject(value: object): boolean {
  const proto = Object.getPrototypeOf(value) as object | null;
  return proto === Object.prototype || proto === null;
}

interface ExtractState {
  readonly attachments: Uint8Array[];
  readonly limits: FramedStreamLimits;
  /** Objects on the current path — detects cycles without rejecting DAGs. */
  readonly ancestors: Set<object>;
}

function extract(value: unknown, state: ExtractState): unknown {
  if (value === null) return null;

  const t = typeof value;
  if (t === 'string' || t === 'boolean') return value;
  if (t === 'number') {
    if (!Number.isFinite(value as number)) {
      throw new FrameProtocolError(`non-finite number cannot cross the wire: ${String(value)}`);
    }
    return value;
  }
  if (t === 'undefined') return undefined;
  if (t === 'bigint' || t === 'function' || t === 'symbol') {
    throw new FrameProtocolError(`unsupported value of type ${t}`);
  }

  if (isSharedBuffer(value)) {
    throw new FrameProtocolError('SharedArrayBuffer cannot cross the wire');
  }

  const bytes = asBytes(value);
  if (bytes !== null) {
    if (state.attachments.length >= state.limits.maxAttachments) {
      throw new FrameProtocolError(
        `too many binary attachments (limit ${state.limits.maxAttachments})`,
      );
    }
    state.attachments.push(bytes);
    return { $mille: 'bin', i: state.attachments.length - 1 } satisfies BinaryPlaceholder;
  }

  const obj = value as object;
  if (state.ancestors.has(obj)) {
    throw new FrameProtocolError('cyclic object cannot be encoded');
  }
  state.ancestors.add(obj);
  try {
    if (Array.isArray(obj)) {
      return obj.map((item) => extract(item, state));
    }
    if (!isPlainObject(obj)) {
      throw new FrameProtocolError(
        `unsupported value with prototype ${Object.getPrototypeOf(obj)?.constructor?.name ?? 'unknown'}`,
      );
    }
    const out: Record<string, unknown> = {};
    for (const [key, item] of Object.entries(obj)) {
      // `undefined` members are dropped by JSON anyway; skipping them here
      // keeps the encoded form identical to what structured clone produced.
      if (item === undefined) continue;
      out[key] = extract(item, state);
    }
    return out;
  } finally {
    state.ancestors.delete(obj);
  }
}

function reconstruct(value: unknown, attachments: readonly Uint8Array[]): unknown {
  if (value === null || typeof value !== 'object') return value;
  if (isBinaryPlaceholder(value)) {
    const { i } = value;
    if (!Number.isInteger(i) || i < 0 || i >= attachments.length) {
      throw new FrameProtocolError(`binary placeholder index ${String(i)} is out of range`);
    }
    return attachments[i];
  }
  if (Array.isArray(value)) {
    return value.map((item) => reconstruct(item, attachments));
  }
  const out: Record<string, unknown> = {};
  for (const [key, item] of Object.entries(value as Record<string, unknown>)) {
    out[key] = reconstruct(item, attachments);
  }
  return out;
}

/**
 * Encode one semantic message into one frame.
 *
 * Throws `FrameProtocolError` when the message cannot be represented or
 * would exceed a limit — the caller decides whether that closes the channel.
 */
export function encodeFrame(message: unknown, limits: FramedStreamLimits): Uint8Array<ArrayBuffer> {
  const attachments: Uint8Array[] = [];
  const metadata = extract(message, { attachments, limits, ancestors: new Set<object>() });

  const json = JSON.stringify(metadata);
  if (json === undefined) {
    throw new FrameProtocolError('message is not JSON-representable');
  }
  const metaBytes = new TextEncoder().encode(json);
  if (metaBytes.byteLength > limits.maxMetadataBytes) {
    throw new FrameProtocolError(
      `metadata ${metaBytes.byteLength} exceeds limit ${limits.maxMetadataBytes}`,
    );
  }

  let attachmentBytes = 0;
  for (const a of attachments) attachmentBytes += a.byteLength;

  const total = HEADER_BYTES + metaBytes.byteLength + attachments.length * 4 + attachmentBytes;
  if (total > limits.maxFrameBytes) {
    throw new FrameProtocolError(`frame ${total} exceeds limit ${limits.maxFrameBytes}`);
  }

  const out = new Uint8Array(total);
  const view = new DataView(out.buffer);
  out.set(WIRE_MAGIC, 0);
  out[4] = WIRE_MAJOR;
  out[5] = WIRE_MINOR;
  view.setUint16(6, 0, false); // flags — none defined in Phase 1
  view.setUint32(8, metaBytes.byteLength, false);
  view.setUint32(12, attachments.length, false);
  view.setUint32(16, attachmentBytes, false);

  let offset = HEADER_BYTES;
  out.set(metaBytes, offset);
  offset += metaBytes.byteLength;
  for (const a of attachments) {
    view.setUint32(offset, a.byteLength, false);
    offset += 4;
  }
  for (const a of attachments) {
    out.set(a, offset);
    offset += a.byteLength;
  }
  return out;
}

export interface FrameHeader {
  readonly metadataLength: number;
  readonly attachmentCount: number;
  readonly attachmentBytes: number;
  /** Total frame size including the header. */
  readonly totalLength: number;
}

/**
 * Validate a 20-byte header without touching the body.
 *
 * This is the security-critical step: it runs before the decoder buffers
 * the rest of the frame, so a lie in a length field is rejected at a cost
 * of a few comparisons rather than an allocation.
 */
export function parseHeader(header: Uint8Array, limits: FramedStreamLimits): FrameHeader {
  if (header.byteLength < HEADER_BYTES) {
    throw new FrameProtocolError('header shorter than 20 bytes');
  }
  for (let i = 0; i < WIRE_MAGIC.length; i += 1) {
    if (header[i] !== WIRE_MAGIC[i]) {
      throw new FrameProtocolError('bad magic — not a Mille frame');
    }
  }
  const major = header[4]!;
  if (major !== WIRE_MAJOR) {
    throw new FrameProtocolError(`unsupported wire major ${major} (expected ${WIRE_MAJOR})`);
  }
  // A higher minor is fine: minor bumps are additive by definition, and any
  // field we do not understand rides inside metadata we already ignore.
  const view = new DataView(header.buffer, header.byteOffset, header.byteLength);
  const flags = view.getUint16(6, false);
  if ((flags & ~KNOWN_FLAGS) !== 0) {
    throw new FrameProtocolError(`unknown active wire flags 0x${flags.toString(16)}`);
  }
  const metadataLength = view.getUint32(8, false);
  const attachmentCount = view.getUint32(12, false);
  const attachmentBytes = view.getUint32(16, false);

  if (metadataLength > limits.maxMetadataBytes) {
    throw new FrameProtocolError(
      `metadata length ${metadataLength} exceeds limit ${limits.maxMetadataBytes}`,
    );
  }
  if (attachmentCount > limits.maxAttachments) {
    throw new FrameProtocolError(
      `attachment count ${attachmentCount} exceeds limit ${limits.maxAttachments}`,
    );
  }
  // Every term is a u32, so the sum is at most ~1.7e10 — comfortably inside
  // Number.MAX_SAFE_INTEGER. No wraparound is possible here.
  const totalLength = HEADER_BYTES + metadataLength + attachmentCount * 4 + attachmentBytes;
  if (totalLength > limits.maxFrameBytes) {
    throw new FrameProtocolError(
      `frame length ${totalLength} exceeds limit ${limits.maxFrameBytes}`,
    );
  }
  return { metadataLength, attachmentCount, attachmentBytes, totalLength };
}

/** Decode a complete, header-validated frame into its semantic message. */
export function decodeFrame(frame: Uint8Array, header: FrameHeader): unknown {
  if (frame.byteLength !== header.totalLength) {
    throw new FrameProtocolError(
      `frame body is ${frame.byteLength} bytes, header declared ${header.totalLength}`,
    );
  }
  let offset = HEADER_BYTES;
  const metaBytes = frame.subarray(offset, offset + header.metadataLength);
  offset += header.metadataLength;

  let json: string;
  try {
    json = new TextDecoder('utf-8', { fatal: true }).decode(metaBytes);
  } catch {
    throw new FrameProtocolError('metadata is not valid UTF-8');
  }
  let metadata: unknown;
  try {
    metadata = JSON.parse(json);
  } catch {
    throw new FrameProtocolError('metadata is not valid JSON');
  }

  const lengths: number[] = [];
  const table = new DataView(frame.buffer, frame.byteOffset + offset, header.attachmentCount * 4);
  let sum = 0;
  for (let i = 0; i < header.attachmentCount; i += 1) {
    const len = table.getUint32(i * 4, false);
    lengths.push(len);
    sum += len;
  }
  offset += header.attachmentCount * 4;
  if (sum !== header.attachmentBytes) {
    throw new FrameProtocolError(
      `attachment table sums to ${sum}, header declared ${header.attachmentBytes}`,
    );
  }

  const attachments: Uint8Array[] = [];
  for (const len of lengths) {
    // Copy rather than subarray: the caller keeps these past the lifetime of
    // the decoder's buffer, and a retained view would pin the whole frame.
    attachments.push(Uint8Array.prototype.slice.call(frame, offset, offset + len));
    offset += len;
  }

  return reconstruct(metadata, attachments);
}
