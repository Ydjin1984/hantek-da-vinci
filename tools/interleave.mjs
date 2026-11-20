// Dev probe: what does the device's raw short stream look like, and at which
// group size are the enabled channels multiplexed?
//
// The daemon hides the bytes: demux() splits the stream by `enabled.len()`. We
// can put the stream back together from a frame whose columns map 1:1 to
// samples (width == n), then measure which lag gives the smoothest series.
//
//   node tools/interleave.mjs            # current channel set
//   node tools/interleave.mjs 1,2,3      # switch channels first, then probe
const base = process.argv[3] ?? "http://127.0.0.1:7411";
const want = process.argv[2];

if (want) {
  await fetch(`${base}/api/control?cmd=channels&list=${want}`);
  await new Promise((r) => setTimeout(r, 600));
}

async function header() {
  const r = await fetch(`${base}/api/frame?width=64&span=0.0016666666666666668`);
  const b = Buffer.from(await r.arrayBuffer());
  const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
  return {
    n: dv.getUint32(28, true),
    nch: dv.getUint32(24, true),
    mask: dv.getUint32(8, true),
    dtNs: dv.getFloat32(12, true),
  };
}

async function series(width) {
  const r = await fetch(`${base}/api/frame?width=${width}&span=0.0016666666666666668`);
  const b = Buffer.from(await r.arrayBuffer());
  const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const nch = dv.getUint32(24, true);
  const out = [];
  let off = 32;
  for (let c = 0; c < nch; c++) {
    const hi = new Float32Array(width);
    for (let x = 0; x < width; x++) {
      hi[x] = dv.getInt16(off + 2, true) / 1000;
      off += 4;
    }
    out.push(hi);
  }
  return out;
}

const h = await header();
console.log({
  mask: h.mask.toString(2),
  nch: h.nch,
  samples_per_channel: h.n,
  total_shorts: h.n * h.nch,
  dt_ns: h.dtNs,
  order: [...Array(h.nch).keys()].map((c) => `CH${Math.log2(h.mask & -h.mask) + c + 1}`),
});

const ch = await series(h.n);
// ch[k][i] came from stream position i*nch + k
const raw = [];
for (let i = 0; i < h.n; i++) for (let k = 0; k < h.nch; k++) raw.push(ch[k][i]);

console.log("first 24 stream values:", raw.slice(0, 24).map((v) => v.toFixed(2)).join(" "));

for (let p = 1; p <= 6; p++) {
  let sum = 0;
  let big = 0;
  const cnt = raw.length - p;
  for (let i = 0; i < cnt; i++) {
    const d = Math.abs(raw[i + p] - raw[i]);
    sum += d;
    if (d > 0.5) big++;
  }
  console.log(`lag ${p}: mean|d|=${(sum / cnt).toFixed(3)} jumps>0.5V=${((big / cnt) * 100).toFixed(1)}%`);
}

// If a phase is a padding or a duplicate slot it stays nearly constant while
// the real channels move together.
for (const p of [h.nch, 4, 8]) {
  const phases = [];
  for (let k = 0; k < p; k++) {
    let mn = Infinity, mx = -Infinity, flat = 0, prev = null, n2 = 0;
    for (let i = k; i < raw.length; i += p) {
      const v = raw[i];
      mn = Math.min(mn, v);
      mx = Math.max(mx, v);
      if (prev !== null) { if (Math.abs(v - prev) < 0.05) flat++; n2++; }
      prev = v;
    }
    phases.push(`k${k}:${mn.toFixed(2)}..${mx.toFixed(2)} flat=${n2 ? ((flat / n2) * 100).toFixed(0) : "-"}%`);
  }
  console.log(`period ${p}: ${phases.join(" ")}`);
}
