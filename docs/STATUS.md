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

## M1 — acquisition and PL sync (done, 2026-10-08)

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

### Done: carrier recovery, generic PSK, narrow carriers (2026-10-08)

From live Ku-band use:

- **Carrier recovery** (`decdvb-dsp::carrier`): a second-order PLL,
  decision-directed or data-aided. Lock is judged by *coherence*,
  |⟨e^{jS·err}⟩| folded by the constellation's symmetry — MER alone cannot
  tell (a rotating QPSK ring still reads ~7 dB, 8PSK ~13 dB).
- **DVB-S2 VFOs** (`demod`): the loop runs over each frame as it is emitted.
  Data-aided over the PLHEADER and pilots, so the absolute phase is resolved;
  decision-directed over the data against that frame's MODCOD constellation,
  so ACM stays locked across QPSK/8PSK/APSK. Seeded at acquisition from the
  two confirming headers (lag-1/8/32 cascade on the de-modulated header, near
  the CRB) or from Identify's offset when the headers agree. `PlFrame.payload`
  is now carrier-corrected — ready for M2.
- **Identify** shows a locked constellation and MER: SOF-summed coarse offset
  for DVB-S2 (else the power line), then the same PLL.
- **Generic PSK → symbols** (`psk`): blind acquisition through Identify, then
  MF → AGC → Gardner → PLL; hard decisions to a `.bin`, one byte per symbol.
- **Narrow carriers**: VFOs down to 500 Hz; demodulator acquisition now takes
  Identify's first look (≤ 2 s) instead of a fixed 300 000 samples, which on a
  10 kBd VFO had meant 6–12 s before anything happened. Identify's in-band
  floor moved to the 3rd percentile — a carrier filling its VFO had read as
  "no signal".
- **GUI**: SDR++-style frequency readout across the top; ✕ on VFO labels and
  list rows. egui's proportional font lacks →, ✕ and ●, which rendered as
  boxes: the X is now drawn, and the bundled monospace font is a fallback.

Tests: a 10 kBd QPSK carrier with 150 Hz LNB error and phase noise through a
narrow VFO; 8PSK through a VFO to a `.bin`; an ACM sequence locked frame by
frame with pilots on their true phase; a wrong seed overruled by the headers.

### M1 complete

Every M1 item is in: RRC, timing, carrier recovery, PLHEADER sync and decode,
descrambling, pilot tracking, a locked constellation.

### Still to do

1. DC/IQ-imbalance correction for the HackRF's DC spur.
2. NCO mixing per VFO in f32 / lane-parallel (it is f64 per sample now).
3. The carrier loop's nearest-point search is linear in the constellation
   size; a sector lookup would cut 32APSK's cost.

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

## M2 — FEC core (done, 2026-10-08)

PLFRAMEs now decode to BBFRAMEs, for all 21 DVB-S2 codes (normal and short).

- **LDPC** (`decdvb-fec::ldpc`): the Annex B/C address tables (generated from
  gr-dvbs2rx's copy of the standard's), an encoder, and a layered normalized
  min-sum decoder over the codes' quasi-cyclic structure — q layers of 360
  checks, each a block of lane-parallel arithmetic that vectorises. 8-bit
  messages, 16-bit posteriors: with 8-bit posteriors a saturated value stops
  being the sum of its messages and decoding can diverge outright (measured:
  36 000 bit errors where 16-bit converges). Normalization 7/8 (3/4 left
  rates 1/4 and 1/3 stuck a few bits short). A decode stuck on a couple of
  parity checks — two adjacent accumulator bits both wrong, which min-sum
  holds forever — stops early and leaves the verdict to BCH. ~0.5 ms per
  iteration per normal frame (release); QPSK 1/2 at 1.5 dB Eb/N0 converges in
  ~12 iterations.
- **BCH** (`bch`): GF(2^16) / GF(2^14), generator built from the minimal
  polynomials of α, α³, … (checked against Tables 6a/6b); a byte-wise
  remainder says "clean" in one pass, else syndromes, Berlekamp–Massey and a
  Chien search over the shortened positions.
- **Demapping** (`demap`): the Table 8 interleaver (8PSK 3/5 reads columns
  2,1,0), max-log LLRs, scaled by the gain and noise measured on each
  frame's header and pilots.
- **BBFRAMEs** (`decdvb-frame::bbframe`): BB descrambling (sequence checked
  against Figure 5: `03 F6 08 34 …`), BBHEADER parse with CRC-8 (check value
  0xBC), normal vs high-efficiency mode read from CRC vs CRC ⊕ 1.
- **Transmit side** (`decdvb-mod`): TS-mode BBFRAMEs (CRC-8 replacing sync
  bytes, SYNCD), BCH and LDPC encoders; `PlFramer` builds real FECFRAMEs, so
  test signals and the `scene` capture decode end to end.
- **VFOs**: each DVB-S2 VFO runs FEC on its own thread behind a bounded queue
  (frames dropped and counted if it falls behind); the side bar shows good
  BBFRAMEs, LDPC iterations, BCH corrections, Es/N0, the stream (TS/GS,
  CCM/ACM, ISI, roll-off) and the payload rate.

**What FEC exposed.** The first end-to-end run failed every QPSK frame below
6 dB Es/N0, though LDPC alone works at 1 dB. Causes, in order of impact:

1. The PLS code was read differentially with hard decisions — a QPSK 1/2
   header read as 3/5 (same frame length, so the grid still held and FEC ran
   the wrong code). Headers are now read coherently (phase anchored on the
   SOF, soft decisions) once the carrier loop runs, and confirmed by coherent
   correlation; acquisition reads both headers coherently from an SOF-only
   frequency estimate.
2. Cycle slips in the decision-directed carrier loop: the phase is now
   re-anchored on every header and pilot block, and the loop bandwidth follows
   the measured Es/N0 (0.01 → 0.002), seeded from the headers at acquisition.
3. The timing loop narrows once frames are locked.

Result: QPSK 1/2 with pilots decodes every frame at 2 dB Es/N0 (threshold
~1 dB), Es/N0 read within 0.2 dB. Without pilots: clean at 5 dB, about one
frame in seven lost to a slip at 4 dB — what pilots are for. An ACM sequence
(QPSK 1/2, 8PSK 3/5, short 16APSK 2/3, short 32APSK 3/4) decodes bit-exact
against the BBFRAMEs that were sent.

Also in this round: the generic PSK decoder writes only while Record is on;
Identify keeps a live constellation between identifications; middle/right
drag moves the spectrum and, past the span's edge, tunes a live HackRF.

### Next

- M4: GSE → IP → PCAP (dontlookup's quirks), TS output — the FEC thread is
  where they hook in.
- M3: S2X codes and constellations.
- FEC throughput for wide carriers: explicit SIMD, or a decoder pool.
- Cycle-slip resistance without pilots (non-causal phase smoothing between
  headers).
