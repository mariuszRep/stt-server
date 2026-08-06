/**
 * Web Worker for WAV encoding + peak computation.
 * Receives Float32Array (transferable) + sampleRate, returns { blob, peaks }.
 */

self.onmessage = (e) => {
  const { pcm, sampleRate, id } = e.data;
  const peaks = computePeaks(pcm, 80);
  const blob = encodeWav(pcm, sampleRate);
  self.postMessage({ id, blob, peaks });
};

function computePeaks(pcm, targetBuckets = 80) {
  if (pcm.length === 0) return [];
  const bucketSize = Math.max(1, Math.floor(pcm.length / targetBuckets));
  const peaks = [];
  for (let i = 0; i < pcm.length; i += bucketSize) {
    let max = 0;
    const end = Math.min(i + bucketSize, pcm.length);
    for (let j = i; j < end; j++) {
      const abs = Math.abs(pcm[j]);
      if (abs > max) max = abs;
    }
    peaks.push(max);
  }
  return peaks;
}

function encodeWav(pcm, sampleRate) {
  const numChannels = 1;
  const bitsPerSample = 16;
  const bytesPerSample = bitsPerSample / 8;
  const blockAlign = numChannels * bytesPerSample;
  const dataSize = pcm.length * bytesPerSample;
  const buffer = new ArrayBuffer(44 + dataSize);
  const view = new DataView(buffer);

  const writeString = (offset, str) => {
    for (let i = 0; i < str.length; i++) {
      view.setUint8(offset + i, str.charCodeAt(i));
    }
  };

  writeString(0, "RIFF");
  view.setUint32(4, 36 + dataSize, true);
  writeString(8, "WAVE");
  writeString(12, "fmt ");
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, numChannels, true);
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * blockAlign, true);
  view.setUint16(32, blockAlign, true);
  view.setUint16(34, bitsPerSample, true);
  writeString(36, "data");
  view.setUint32(40, dataSize, true);

  let offset = 44;
  for (let i = 0; i < pcm.length; i++) {
    const clamped = Math.max(-1, Math.min(1, pcm[i]));
    view.setInt16(offset, clamped < 0 ? clamped * 0x8000 : clamped * 0x7fff, true);
    offset += 2;
  }

  return new Blob([buffer], { type: "audio/wav" });
}
