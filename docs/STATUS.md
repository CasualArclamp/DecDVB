# Status

Running log of what is done and what is next. `docs/DESIGN.md` holds the plan.

## M0 — skeleton (done, 2026-10-08)

Workspace builds clean, tests and clippy green.

- `decdvb-core` — `Modulation`, `CodeRate`, `FecFrame`, `Modcod` + the 28
  standard DVB-S2 MODCODs; `RollOff` (0.35…0.05); `SampleFormat` (cs8/cs16/cf32)
  and IQ byte conversion; `RxConfig`, `Metrics`, `Error`.
- `decdvb-io` — `IqSource` trait, `IqFileReader` / `IqFileWriter`, extension
  sniffing. HackRF front end scaffolded behind the `hackrf` feature (loads
  `libhackrf` at run time in M1, so no SDK is needed to build).
- `decdvb-dsp` — root-raised-cosine taps (unit energy, singularities handled)
  and a streaming complex FIR.
- `decdvb-engine` — `Spectrum` (Hann, 50 % overlap, averaged, FFT-shifted) and
  `estimate_band`: carrier centre, 99 % occupied bandwidth, noise floor, and a
  **symbol-rate estimate from the equivalent noise bandwidth** — exact for a
  root-raised-cosine signal regardless of roll-off (see the doc comment).
- `decdvb-cli` — `modcods`, `analyse`, and `synth` (a shaped QPSK test-signal
  generator with frequency offset and Es/N0; not a DVB-S2 modulator, that is M6).
- `decdvb-gui` — eframe/glow window: open a capture, spectrum and constellation.
- CI on GitHub Actions: Linux + Windows build/test, clippy, rustfmt, and a build
  with the `hackrf` feature to prove it compiles with no SDR attached.
- Stub crates in place for `fec`, `frame`, `gse`, `ts`, `ip`, `mod`.

Verified by hand: `synth` then `analyse` recovers the carrier offset and symbol
rate of signals at 125 kS/s–1 MS/s with roll-offs 0.05–0.35.

## M1 — acquisition and PL sync (in progress)

### Done: the PLHEADER (`decdvb-frame`)

The 90-symbol PLHEADER is fully implemented and tested — this is the part that
makes ACM possible at all, since every PLFRAME announces its own MODCOD.

- `defs` — PLFRAME geometry (SOF 26, PLSC 64, slot 90, pilot block 36 every
  16 slots), the SOF pattern and the PLS scrambler.
- `pi2bpsk` — pi/2-BPSK map, coherent hard/soft demap, and **differential**
  demap for use before carrier lock.
- `rm` — the interleaved **(64, 7, 32) Reed–Muller** code the PLS code is
  protected by, with hard (Hamming) and soft (max-inner-product) decoding. The
  soft path folds descrambling into the codeword table. Tests confirm minimum
  distance 32 and correction of any 15-bit error pattern.
- `plsc` — `PlsInfo`: MODCOD, FECFRAME length, pilots, and the frame geometry
  they imply (slots, pilot blocks, XFECFRAME and PLFRAME lengths). A test
  checks `slots × 90 × bits_per_symbol` equals the FECFRAME length for all 28
  MODCODs in both frame lengths, so the geometry cannot silently drift.
- `sync` — `PlHeaderCorrelator`, differential correlation against SOF (25 taps)
  plus PLSC (32 taps). The PLSC taps come from the **scrambler alone**: the
  interleaved Reed–Muller construction makes each consecutive codeword bit pair
  either equal or opposite, so the pairwise differential is fixed up to a 180°
  flip that `max(|SOF+PLSC|, |SOF-PLSC|)` resolves. So the correlator finds a
  PLHEADER without knowing its PLS code, and — being differential — works at
  frequency offsets up to at least 1 % of the symbol rate. Verified: correct
  peak position for all 32 MODCODs, under noise, and under offset.

### Done: PL scrambling, timing, streaming demodulator

- `scramble` — Gold-code PL scrambling (§5.5.4); exact round trip, x LFSR
  verified maximal-length.
- `decdvb-dsp::timing` — Gardner TED + cubic Farrow interpolator, at
  fractional samples per symbol; verified from 2.7 up to 84 sps.
- `decdvb-engine::demod` — streaming PL demodulator: matched filter → AGC →
  timing → PLHEADER correlation → **frame lock** → PLS decode → descrambling.
  A frame is emitted only once the *next* header is found exactly where this
  frame's PLS code predicted, which validates the PLS decode and the frame
  length together — what makes following ACM safe. Follows a five-MODCOD ACM
  sequence with zero losses, re-acquires after a dropout, never invents frames
  from noise.

### Done: live HackRF (2026-10-08)

`decdvb-io::hackrf` — pure Rust over USB via `seify-hackrfone` (MIT, nusb):
no libhackrf, no libusb, no DLLs, so the portable exe stays one file, and the
feature is now on by default. Receive only; antenna-port power forced off on
every configuration. A reader thread keeps three 256 KB transfers in flight and
hands buffers over a bounded channel, dropping (and counting) whole buffers if
the consumer falls behind. The driver's gain setters `panic!` on an unexpected
reply, so gains are snapped to valid steps and calls run under `catch_unwind`.
Verified on Rory's HackRF: 7.99 MS/s delivered at 8 MS/s, and the GUI running
live at 20 MS/s with the front end at 1–2 % CPU. GUI: a HackRF panel with
frequency, rate, LNA/VGA/amp and an LNB LO for the axis; frequency and gains
apply live.

**Carrier detection, hardened on real signal.** The live FM band first
produced 168 "carriers": a regular ~100 kHz comb (likely switching-PSU or USB
interference near the PC, or overload; not investigated) whose every tooth
passed the threshold. Now: regions are split only at valleys that are deep
relative to both sides' robust (95th-percentile) tops *and* where both sides
have carrier-like flat tops; narrow lines are recognised by a short top run
that holds most of the power; a region with a non-flat top is flagged `rough`
(orange "lump"); spurs are ticks, not brackets; clean carriers outrank spurs
under the 48-carrier cap. The same band now reads 6 carriers + 42 narrow lines,
the comb as one lump. Six regression tests cover it.

### Still to do

1. Carrier recovery: coarse from the 4th-power line / header phase, fine from
   pilots and a decision-directed loop — the payload still rotates.
2. DC/IQ-imbalance correction for the HackRF's DC spur.
3. NCO mixing per VFO in f32 / lane-parallel (it is f64 per sample now).

## M1b — wideband waterfall + VFOs + Identify (done, 2026-10-08)

Rory asked for an SDR++-style app: a big waterfall, VFOs dropped on it each
running a decoder of choice, and a "what is this?" mode. Design in DESIGN.md
§3a/§3b.

- **Front end** (`frontend`): one thread reads the source in ~1/25 s blocks,
  makes a waterfall row per block, keeps a smoothed spectrum, detects carriers
  twice a second, and shares each block with every VFO by `Arc` (no copies).
  A slow VFO has blocks dropped and counted rather than stalling the rest.
- **VFOs** (`vfo`): one worker thread each; settings travel through a mailbox
  slot so the GUI never blocks on a busy worker (a bounded channel would have).
  Decoders: Identify, DVB-S2 → IP, DVB-S2 → TS (both run the PL demodulator
  until FEC exists), IQ recorder, spectrum only.
- **DDC**: NCO + one or two decimating FIR stages, the split chosen to minimise
  multiply-adds; stopband just past the VFO's edge so a VFO delivers what its
  edges show.
- **Carrier detection** (`carriers`): runs over a 20th-percentile floor (a
  busy span still finds the true noise), ignores the DC spur, separates
  adjacent carriers, estimates each one's symbol rate from its noise bandwidth.
- **Identify** (`identify`): see DESIGN.md §3b. The symbol rate from the cyclic
  line of |x|² is accurate to ~1e-8 on test carriers. Adaptive listening: a
  quick first look, then — if no headers were seen and the carrier is too slow
  to have shown three frames — a longer listen sized from the measured rate,
  the interim verdict marked provisional. Rests 3 s between checks once sure.
- **GUI**: spectrum + ring-texture waterfall, zoom/pan, VFO boxes with live
  badges, green brackets on detected carriers (click to claim), side bar with
  VFO list (CPU per VFO), settings, Identify card, MODCOD table, constellation,
  zoomed spectrum. Unattended mode for screenshots/smoke tests:
  `--claim-carriers --select N --after S --screenshot out.png`.
- **CLI**: `scene` (the multi-carrier test capture), `scan` (find and identify
  every carrier).

Verified on the `scene` capture: all four carriers found (centres within
0.2 kHz, symbol rates within 1–2 %) and all four identified correctly — DVB-S2
CCM QPSK 1/2; DVB-S2 ACM QPSK 1/2 → 8PSK 3/5 → 16APSK 2/3; plain QPSK as
"possibly DVB-S (not verified)"; a CW as a narrow carrier.

Bugs found by that real run and fixed, each with a regression test:
- the DDC stopband sat at the aliasing limit, passing a neighbour ~1.7 MHz away
  into a 1.5 MHz VFO and collapsing the symbol-rate estimate to 47.5 kS/s;
- the timing loop's buffer trim overran at > 8 samples/symbol (panic);
- the timing loop's gains were in samples, not symbols, so it ran 1/sps too
  slow — 84× at 84 sps;
- an Identify VFO dragged to another carrier kept analysing the old signal.

CPU per VFO on the scene went from 45/3/30/38 % to 11/3/8/6 % (two-stage DDC,
vectorisable FIR inner loop, Identify resting once sure). Measured and
rejected: AVX2 run-time dispatch with a flat tap layout — slower on these
window lengths (notes in `decdvb-dsp::dot`).
