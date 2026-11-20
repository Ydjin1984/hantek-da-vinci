// Dev probe: map how the 1008 multiplexes channels for each channel count.
//
// For every channel set it enables, reads one frame and reports the lag at
// which the raw short stream becomes smooth — that lag is the true group size,
// which is what demux() must assume.
const base = process.argv[2] ?? "http://127.0.0.1:7411";
const sets = process.argv.slice(3);

async function frame(width) {
  const r = await fetch(`${base}/api/frame?width=${width}&span=0.0016666666666666668`);
  const b = Buffer.from(await r.arrayBuffer());
  return new DataView(b.buffer, b.byteOffset, b.byteLength);
}

for (const set of sets) {
  await fetch(`${base}/api/control?cmd=channels&list=${set}`);
  await new Promise((r) => setTimeout(r, 700));

  const h = await frame(64);
  const nch = h.getUint32(24, true);
  const n = h.getUint32(28, true);
  const mask = h.getUint32(8, true);
  const chans = [];
  for (let c = 0; c < 8; c++) if (mask & (1 << c)) chans.push(c + 1);

  const d = await frame(n);
  const cols = [];
  let off = 32;
  for (let c = 0; c < nch; c++) {
    const hi = new Float32Array(n);
    for (let x = 0; x < n; x++) {
      hi[x] = d.getInt16(off + 2, true) / 1000;
      off += 4;
    }
    cols.push(hi);
  }
  const raw = [];
  for (let i = 0; i < n; i++) for (let k = 0; k < nch; k++) raw.push(cols[k][i]);

  let best = 1, bestD = Infinity;
  const lags = [];
  for (let p = 1; p <= 8; p++) {
    let sum = 0;
    for (let i = 0; i < raw.length - p; i++) sum += Math.abs(raw[i + p] - raw[i]);
    const m = sum / (raw.length - p);
    lags.push(`${p}:${m.toFixed(3)}`);
    if (m < bestD) { bestD = m; best = p; }
  }

  // spread of each phase at the best lag: a constant phase is a padded slot
  const phase = [];
  for (let k = 0; k < best; k++) {
    let mn = Infinity, mx = -Infinity;
    for (let i = k; i < raw.length; i += best) { mn = Math.min(mn, raw[i]); mx = Math.max(mx, raw[i]); }
    phase.push(`${mn.toFixed(2)}..${mx.toFixed(2)}`);
  }

  const demuxLag = nch;
  console.log(
    `channels [${chans.join(",")}] -> nch=${nch} n=${n} shorts=${n * nch} ` +
    `demux step=${demuxLag} ${demuxLag === best ? "OK" : "MISALIGNED"} ` +
    `true group=${best} phases: ${phase.join(" | ")}  lags ${lags.join(" ")}`,
  );
}
