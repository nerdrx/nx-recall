// The worklet's exact resample + PCM16 + wire-line path, run outside a browser,
// to answer two questions: what does a frame cost on the wire, and what does the
// tap cost the renderer per person in a call.
const HALF_TAPS = 16, PHASES = 128;
const win = t => { const x = (t + 1) / 2; return 0.42 - 0.5 * Math.cos(2 * Math.PI * x) + 0.08 * Math.cos(4 * Math.PI * x); };
const sinc = x => Math.abs(x) < 1e-9 ? 1 : Math.sin(Math.PI * x) / (Math.PI * x);
function kernel(ratio) {
  const cutoff = ratio > 1 ? 1 / ratio : 1, taps = 2 * HALF_TAPS, k = new Float32Array(PHASES * taps);
  for (let p = 0; p < PHASES; p++) {
    const frac = p / PHASES; let sum = 0;
    for (let i = -HALF_TAPS + 1; i <= HALF_TAPS; i++) {
      const t = i - frac, v = cutoff * sinc(cutoff * t) * win(t / HALF_TAPS);
      k[p * taps + (i + HALF_TAPS - 1)] = v; sum += v;
    }
    if (sum !== 0) for (let i = 0; i < taps; i++) k[p * taps + i] /= sum;
  }
  return k;
}
const IN = 48000, OUT = 16000, QUANTUM = 128, FRAME = OUT / 2; // 500 ms
const ratio = IN / OUT, K = kernel(ratio), taps = 2 * HALF_TAPS;
let hist = new Float32Array(0), pos = HALF_TAPS, frame = new Float32Array(FRAME), filled = 0;
let frames = 0, pcmBytes = 0, jsonBytes = 0;

function emit() {
  const pcm = new Int16Array(filled);
  for (let i = 0; i < filled; i++) { let s = frame[i]; s = s > 1 ? 1 : s < -1 ? -1 : s; pcm[i] = Math.round(s * 32767); }
  const b64 = Buffer.from(pcm.buffer).toString('base64');
  const line = JSON.stringify({
    t_ms: 1788201960000, user_id: "123456789012345678", name: "Aspen",
    channel_id: "987654321098765432", rate: OUT, seq: frames, samples: filled, pcm: b64
  });
  frames++; pcmBytes += pcm.byteLength; jsonBytes += line.length + 1;
  filled = 0;
}
function run(mono) {
  const carried = hist.length;
  const ext = new Float32Array(carried + mono.length);
  ext.set(hist, 0); ext.set(mono, carried);
  const limit = ext.length - HALF_TAPS;
  while (pos < limit) {
    const base = Math.floor(pos), off = (((pos - base) * PHASES) | 0) * taps;
    let acc = 0;
    for (let i = 0; i < taps; i++) acc += K[off + i] * ext[base - HALF_TAPS + 1 + i];
    frame[filled++] = acc; pos += ratio;
    if (filled === FRAME) emit();
  }
  const keepFrom = Math.max(0, Math.floor(pos) - HALF_TAPS + 1);
  hist = ext.slice(keepFrom); pos -= keepFrom;
}

const SECONDS = 60, quanta = (IN * SECONDS) / QUANTUM;
const q = new Float32Array(QUANTUM);
let ph = 0;
const t0 = process.hrtime.bigint();
for (let n = 0; n < quanta; n++) {
  for (let i = 0; i < QUANTUM; i++) { q[i] = 0.3 * Math.sin(ph) + 0.1 * Math.sin(ph * 3.7); ph += 2 * Math.PI * 220 / IN; }
  run(q);
}
const ms = Number(process.hrtime.bigint() - t0) / 1e6;
console.log(JSON.stringify({
  audio_s: SECONDS, frames, cpu_ms: +ms.toFixed(1),
  cpu_pct_of_one_core: +((ms / (SECONDS * 1000)) * 100).toFixed(3),
  pcm_bytes_per_frame: pcmBytes / frames,
  json_bytes_per_frame: Math.round(jsonBytes / frames),
  kB_per_s_per_person: +((jsonBytes / SECONDS) / 1024).toFixed(1),
  four_people_500ms_post_kB: +((4 * (jsonBytes / frames)) / 1024).toFixed(1)
}, null, 2));
