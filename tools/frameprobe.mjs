// Dev probe: pull one frame from the running daemon and look at the samples.
const span = process.argv[2] ?? "0.0016666666666666668";
const width = Number(process.argv[3] ?? 1999);

const r = await fetch(`http://127.0.0.1:7411/api/frame?width=${width}&span=${span}`);
const buf = Buffer.from(await r.arrayBuffer());
const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
const magic = buf.toString("latin1", 0, 4);
const w = dv.getUint32(4, true);
const mask = dv.getUint32(8, true);
const dtNs = dv.getFloat32(12, true);
const nch = dv.getUint32(24, true);
const n = dv.getUint32(28, true);

console.log({ magic, width: w, mask: mask.toString(2), dtNs, nch, n, bytes: buf.byteLength });

let off = 32;
for (let i = 0; i < nch; i++) {
  const lo = new Float32Array(w);
  const hi = new Float32Array(w);
  for (let x = 0; x < w; x++) {
    lo[x] = dv.getInt16(off, true) / 1000;
    hi[x] = dv.getInt16(off + 2, true) / 1000;
    off += 4;
  }
  const ch = Math.log2(mask & -mask) + i + 1;
  let mn = Infinity, mx = -Infinity, sum = 0, sq = 0;
  let jumps = 0;
  for (let x = 0; x < w; x++) {
    mn = Math.min(mn, lo[x]);
    mx = Math.max(mx, hi[x]);
    sum += hi[x];
    sq += hi[x] * hi[x];
    if (x && Math.abs(hi[x] - hi[x - 1]) > 0.5) jumps++;
  }
  console.log(`CH${ch}: min=${mn.toFixed(3)} max=${mx.toFixed(3)} mean=${(sum / w).toFixed(3)} rms=${Math.sqrt(sq / w).toFixed(3)} jumps>0.5V=${jumps}`);
  if (i === 0) {
    const head = [];
    for (let x = 0; x < Math.min(60, w); x++) head.push(hi[x].toFixed(2));
    console.log("first 60 samples:", head.join(" "));
    const mid = [];
    for (let x = Math.floor(w / 2); x < Math.floor(w / 2) + 60; x++) mid.push(hi[x].toFixed(2));
    console.log("middle 60:", mid.join(" "));
    // per-column spread tells whether the comb is inside a column or between them
    let wide = 0;
    for (let x = 0; x < w; x++) if (hi[x] - lo[x] > 0.5) wide++;
    console.log(`columns with spread > 0.5 V: ${wide} of ${w}`);
  }
}
