import { Stretch } from "./types";
import { buildSegments, stretchedDuration } from "./timeMapping";

/**
 * Decode an audio file from a URL into an AudioBuffer.
 * Uses a temporary AudioContext just for decoding then closes it.
 */
export async function decodeAudioFile(url: string): Promise<AudioBuffer> {
  const response = await fetch(url);
  const arrayBuffer = await response.arrayBuffer();
  const ctx = new AudioContext();
  try {
    return await ctx.decodeAudioData(arrayBuffer);
  } finally {
    ctx.close();
  }
}

/**
 * Build a new AudioBuffer with all stretches applied using OfflineAudioContext.
 * Each normal segment plays at 1×; each stretch segment plays at 1/factor
 * (pitch-shifts, consistent with the live preview in step 4).
 */
export async function buildStretchedBuffer(
  original: AudioBuffer,
  stretches: Stretch[],
): Promise<AudioBuffer> {
  const totalOrigDur = original.duration;
  const totalStretchedDur = stretchedDuration(totalOrigDur, stretches);
  const sampleRate = original.sampleRate;
  const nChannels = original.numberOfChannels;
  const totalSamples = Math.ceil(totalStretchedDur * sampleRate);

  const offCtx = new OfflineAudioContext(nChannels, totalSamples, sampleRate);
  const segments = buildSegments(totalOrigDur, stretches);

  for (const seg of segments) {
    const src = offCtx.createBufferSource();
    src.buffer = original;
    src.playbackRate.value = 1 / seg.factor; // factor > 1 → slower rate
    src.connect(offCtx.destination);
    // start(when, offset, duration) — all in source-buffer seconds
    src.start(seg.stretchedStart, seg.origStart, seg.origEnd - seg.origStart);
  }

  return offCtx.startRendering();
}

/**
 * Encode an AudioBuffer as a 16-bit PCM WAV file.
 * Returns a Uint8Array ready to write to disk.
 */
export function encodeWav(buffer: AudioBuffer): Uint8Array {
  const numChannels = buffer.numberOfChannels;
  const sampleRate = buffer.sampleRate;
  const numFrames = buffer.length;
  const bitsPerSample = 16;
  const bytesPerSample = bitsPerSample / 8;
  const blockAlign = numChannels * bytesPerSample;
  const byteRate = sampleRate * blockAlign;
  const dataSize = numFrames * blockAlign;
  const headerSize = 44;
  const totalSize = headerSize + dataSize;

  const view = new DataView(new ArrayBuffer(totalSize));

  function writeStr(offset: number, str: string) {
    for (let i = 0; i < str.length; i++) view.setUint8(offset + i, str.charCodeAt(i));
  }
  function writeU16(offset: number, v: number) { view.setUint16(offset, v, true); }
  function writeU32(offset: number, v: number) { view.setUint32(offset, v, true); }

  // RIFF header
  writeStr(0, "RIFF");
  writeU32(4, totalSize - 8);
  writeStr(8, "WAVE");
  // fmt chunk
  writeStr(12, "fmt ");
  writeU32(16, 16);         // chunk size
  writeU16(20, 1);          // PCM
  writeU16(22, numChannels);
  writeU32(24, sampleRate);
  writeU32(28, byteRate);
  writeU16(32, blockAlign);
  writeU16(34, bitsPerSample);
  // data chunk
  writeStr(36, "data");
  writeU32(40, dataSize);

  // Interleave channels and write samples
  let offset = headerSize;
  for (let i = 0; i < numFrames; i++) {
    for (let c = 0; c < numChannels; c++) {
      const sample = Math.max(-1, Math.min(1, buffer.getChannelData(c)[i]));
      const int16 = sample < 0 ? sample * 0x8000 : sample * 0x7fff;
      view.setInt16(offset, int16, true);
      offset += 2;
    }
  }

  return new Uint8Array(view.buffer);
}
