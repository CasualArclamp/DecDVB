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

## M4, part 1 — GSE → IP → PCAP (done, 2026-10-08)

- **`decdvb-ip`**: IPv4/IPv6 validation (IPv4 header checksum, lengths),
  a classic PCAP writer (LINKTYPE_RAW), live statistics (protocols, a
  bounded flow table, top talkers) and the blind IPv4 search.
- **`decdvb-gse`**: a GSE decoder per variant — GSE_LENGTH standard or
  counting the header (dontlookup's "hdrlen"), frag id 8-bit or 6+2 split —
  with reassembly across BBFRAMEs, label types incl. re-use, padding, and the
  CRC-32/MPEG-2 over total length, protocol type, label and PDU. `GseIp` runs
  all four on every data field, scores each by valid IP and passes on the
  best one's packets; a blind IPv4 search takes over if none yields IP for
  50 fields. Bridged-Ethernet PDUs (type 0x0001) give up their IP. An
  encapsulator produces any variant, for tests and the modulator.
- **Engine**: good generic-stream BBFRAMEs (UPL 0) go from the FEC thread
  through `GseIp`; **● Record** writes PCAP (a new file per recording, with
  no restart). The GSE variant can be forced per VFO.
- **Test signals**: `GseBbFramer` puts UDP flows between RFC 5737
  documentation addresses into GSE in any variant; the `scene` capture's
  1 MS/s carrier now carries IP over GSE.

Verified: a QPSK 3/4 short-frame carrier carrying GSE written the
non-standard way (header-inclusive length, split frag ids) through a VFO —
the variant found from the data, fragments reassembled with CRCs matching,
and a PCAP whose every record parses as IP. In the GUI on the scene: 389 of
389 BBFRAMEs, 782 IPv4/UDP packets at 847 kbit/s.

Note: the CRC-32 coverage and total-length meaning follow TS 102 606-1 as
read here, and match this encoder; dontlookup computes the CRC but ignores
mismatches, so real links will show whether they agree (the GSE variants
table in the side bar counts CRC ok / bad).

### Next

- MPEG-TS output from TS-mode BBFRAMEs (`decdvb-ts`): sync-byte restore
  from the CRC-8s, null-packet re-insertion, `.ts` file / UDP.
- Live confirmation on a real GSE carrier.

## M4, part 2 — MPEG-TS out (done, 2026-10-08)

- **`decdvb-ts`**: TS-mode data fields back to 188-byte packets. The
  deframer treats the data fields as one stream and locks where SYNCD says,
  verified by the CRC-8 chain (each packet's sync position holds the CRC of
  the one before); if the chain fails there it searches every offset —
  which also reads links whose encoders get SYNCD wrong (dontlookup's
  "generic" and "Newtec" parsers). Sync bytes restored, failed CRCs flagged
  in the transport error indicator, NPD null packets re-inserted, ISSY
  counted (not read yet). A frame the demodulator, queue or FEC lost breaks
  the stream: `PlFrame::after_gap`, queue drops and failed frames all
  trigger a resync.
- **Analysis**: per-PID packets, continuity errors and scrambling; PAT, PMT
  and SDT (with CRC-32) for programme numbers, stream types and service
  names (DVB text, UTF-8 or Latin-1).
- **Outputs** from the FEC thread, switchable live: `.ts` file; UDP (7
  packets per datagram); a TCP server that answers HTTP so VLC and PotPlayer
  open `http://127.0.0.1:8001/`, each client on its own thread with a bounded
  queue. ▶ VLC / ▶ PotPlayer start the server and launch the player once it
  is up.
- **Test signals**: the TS carrier carries a PAT, PMT and SDT ("DecDVB test
  signal") every 40 packets.

Verified: a QPSK 1/2 TS carrier through a VFO to a UDP socket — every
datagram 7 packets with sync bytes, no CRC or continuity errors, the
service name read from the SDT; the TCP server tested with a raw and an
HTTP client on loopback. GUI on the scene: 5896 TS packets at 680 kbit/s.

Also: the toolbar's 📁 Output folder (remembered in
`%APPDATA%\DecDVB\prefs.txt`), LNB LO default 10700 MHz.

## M4, part 3 — TS analyser, MPE, multicast audio (done, 2026-10-08)

- **TS analyser** (`decdvb-ts::psi`, GUI `ts_viewer`): per-PID rate, PCR,
  PES stream id, CC/TEI errors, scrambling; PAT, CAT (EMM), PMT (languages,
  AC-3/E-AC-3/AAC, subtitles, teletext, CA systems → ECM PIDs), SDT, NIT
  (network name, satellite transponders), EIT present/following, TDT/TOT.
  An EBSPro-style window: sortable PID table with share bars, service tree
  with now/next, NIT transponders, tables seen.
- **MPE** (`decdvb-ts::mpe`, EN 301 192 §7): DSM-CC private sections found on
  any unscrambled non-PES PID by their table id; IP datagrams (LLC/SNAP or
  not) go to the same IP stage as GSE — statistics and multicast audio work
  for TS carriers too. Section reassembly is shared (`section.rs`).
- **Multicast audio** (`decdvb-ip::mcast`, `relay`): UDP multicast flows;
  RTP recognised by a steady SSRC and counting sequence numbers; codecs from
  sync words (ADTS, LOAS, MPEG audio) or RTP formats (RFC 3640, PT 14); SAP
  announcements' SDP names and describes streams. Playback: described RTP is
  relayed to a local port with an SDP file for VLC/PotPlayer; a bare
  elementary stream is served over local HTTP. `decdvb-ip::serve` is the
  shared HTTP/TCP streaming server (TS uses it too).
- **Test signals**: a multicast "test radio" (RTP MPEG audio of silent layer
  II frames, announced by SAP) on the GSE carrier, and in MPE on the TS one.

Bugs found on the way, each with a test: a relay started before a stream was
classified kept the wrong mode for good (now waits for 8 packets); stale
audio and top-flow lists after a burst; and the demodulator ran its timing
loop across a whole input block before frame logic could re-widen it after a
lost lock — one call with everything after a dropout never re-acquired (it
now works in 16 k-sample chunks).

## M3, part 1 — S2X FEC layer (done, 2026-10-08)

- The 31 S2X LDPC codes for normal and short FECFRAMEs (EN 302 307-2
  Annex B/C, generated from gr-dvbs2rx's tables: rates 2/9 … 154/180, the
  "-L" rates 90/180 … 22/30 kept unreduced), plus the three medium-frame
  tables for VL-SNR. K_bch/N_bch/t for each (all t = 12).
- Encoder, layered decoder and BCH tested over every S2X code exactly as
  over the S2 ones (codewords check, decoding near threshold, t errors
  corrected).

**What S2X still needs — and why it is not done yet.** The reference code
on hand stops at the FEC layer: none of it has the S2X physical layer — the
8-bit PLS code and the MODCOD 128–255 table, the 8/16/32/64/128/256APSK
constellations (radii and bit labels), the S2X bit-interleaver permutations,
VL-SNR headers and pi/2-BPSK spreading, or superframing. Those come from
EN 302 307-2 itself (or gr-dtv's S2X modulator); they are tables where a
guess would silently decode nothing, so they wait for the source.

## M4, part 4 — multicast audio in the app (done, 2026-10-08)

Rory asked for the multicast radio to decode in DecDVB itself, with a volume
slider and playback controls, the list sorted by address, and a file output.

- **`decdvb-audio`** (new crate). `depay`: UDP payloads to frames — MPEG audio,
  ADTS and LOAS byte streams re-framed by sync word and length (trusted only
  when the next header follows), RFC 2250's header dropped for payload type
  14, MP4A-LATM in RTP (RFC 3016, config in band or from `config=`, fragments
  joined on the marker bit), RFC 3640 AU headers, L16/L24/L8 and G.711.
  `aac`: AudioSpecificConfig (explicit and backward-compatible SBR/PS
  signalling), StreamMuxConfig, ADTS headers. `decode`: Symphonia 0.6
  (MPL-2.0) for MPEG audio I–III and AAC-LC, everything to stereo. `resample`:
  32-tap windowed sinc, ratio trimmable ±1 %. `player`: its own thread, a
  ring buffer to cpal 0.18 (WASAPI/ALSA), 400 ms start level held by trimming
  the ratio against clock drift, app-wide volume (cubed) and mute, per-player
  pause that resumes live, peak meter. `record`: as broadcast — `.mp2/.mp3`,
  ADTS `.aac` for any AAC carriage, `.wav`, `.ts`.
- **HE-AAC** plays its AAC-LC core (Symphonia has no SBR): the decoder is
  given an LC config for the core and ignores the SBR fill elements. Full
  sound: "… → Open in VLC", or the recording.
- **Engine**: `audio_play` + `audio_external` choose the in-app player or the
  external relay; `audio_record` records to the output folder. Under test the
  player uses a null output, so `cargo test` never makes a sound.
- **GUI**: per stream ▶ Play / ⏸ Pause / ⏹ Stop / ⏺ Record, level meter, a
  "…" menu for VLC/PotPlayer; volume slider and mute on the list header,
  saved in prefs. `--play-audio` plays the selected VFO's first stream in
  unattended runs. Streams are listed by group address and port.
- The synthetic scene's test radio is silent and slower than real time, so
  there it reads "buffering · dropouts"; a real stream does not.
- CI installs `libasound2-dev` for cpal on Linux.

**Also (2026-10-08):** a VFO dropped on a detected carrier is now sized to its
occupied width plus about 12 % (was 25 %, or 1.5 Rs), and stops short of the
nearest neighbouring carrier (`Carrier::vfo_bandwidth_among`), never narrower
than the carrier itself: close carriers no longer end up inside each other's VFOs.

## M3, part 2 — the S2X physical layer for normal and short frames (done, 2026-10-08)

With Rory's OK, EN 302 307-2 V1.3.1 (the ETSI PDF) and GNU Radio's `gr-dtv`
(sparse checkout) are in `reference/`. Tables were read out of the PDF with
PyMuPDF's table finder and turned into Rust by scripts, not retyped; gr-dtv
was the cross-check.

- **PLS code, 8 bits** (§5.5.2): the S2X generator row 0x90AC2DDD (Figure 20)
  on top of S2's (64,7) code — distance 32 among S2 codes, 24 across all 256.
  An S2X header's 64 PLS symbols are turned +90° against the SOF; the
  coherent reads try both ways (S2 codewords plainly, S2X ones turned back),
  the differential read takes the first bit both ways. The frame-sync
  correlator needed nothing: it only uses differentials within symbol pairs.
- **`PlsInfo`** holds the 8-bit code, the FECFRAME and `modcod()`; S2X codes
  map to Table 17a MODCODs, VL-SNR frames (129, 131: the lengths of normal
  QPSK/16APSK with pilots, so lock holds), and the Table 17b reserved codes
  with their stated lengths. 128APSK takes 103 slots.
- **`decdvb_core::modcod`**: one numbering for S2 (1–28) and S2X (PLS code,
  132–248); `Display` gives the canonical names ("16APSK 1/2-L").
- **Constellations**: 2+4+2 8APSK, 8+8 16APSK (by γ, and the 18/30, 20/30
  point tables), 4+12 16APSK with S2X ratios, 4+12+16rb and 4+8+4+16 32APSK,
  16+16+16+16, 8+16+20+20 and 4+12+20+28 64APSK, 128APSK, 256APSK on rings
  and the 20/30, 22/30 point tables. Tests: unit power, ring populations
  match each name, every table row's angles are φ, −φ, π−φ, π+φ, labels
  agree with gr-dtv where checked. Table 15d's 256APSK has pairs of points
  0.0001 apart — in the standard and in gr-dtv; not an error.
- **Interleavers** (Tables 9a/9b) per MODCOD; `demap::Mapper` bundles
  constellation, interleaver and 128APSK's padding (6 zero bits, 12 all-ones
  symbols).
- **Modulator**: `FrameSpec::s2x(pls, pilots)`; reserved codes get a random
  payload of the right length. **Test scene**: carrier B is S2/S2X ACM.
- **Tests**: every S2X MODCOD maps and demaps noiselessly; an ACM carrier
  through ten S2X constellation families plus S2 QPSK and a reserved code
  decodes, at 30 dB, to exactly the BBFRAMEs sent. In the GUI the scene's S2X
  carrier decodes 100 %, and Identify calls it DVB-S2X with the MODCOD table.

**Still to do for S2X:** VL-SNR (header with Walsh–Hadamard codes, extra
pilots, pi/2-BPSK with spreading, shortening and puncturing, medium
FECFRAMEs with GF(2^15) BCH), the other PL scrambling sequences of Table 19e
in acquisition, and superframing (Annex E; DESIGN puts it in M5).

## M3, part 3 — S2X VL-SNR (done, 2026-10-08)

- **Medium FECFRAMEs**: BCH over GF(2^15) (g1 = 0x802D; the built generator
  equals the product of Table 7's g1…g12), with messages that are not whole
  bytes (K = 5660, 7740, 10 620: 180 parity bits); the medium LDPC tables
  were already in from part 1.
- **VL-SNR header** (`decdvb-frame::vlsnr`, §5.5.2.5): 16 base rows of 56
  bits signed by Walsh–Hadamard rows — generated from the standard's text,
  and all 16 resulting sequences equal gr-dtv's `ph_vlsnr_seq`. Decoded by
  correlating row by row (896 bits: decodes at −3 dB in a test). The frame
  layout of Figures 17/18 (extra pilot blocks of 34/36 and 32/36 symbols
  mid-group), cross-checked against gr-dtv's pilot insertion.
- **VL-SNR codes** (`decdvb-fec::vlsnr`, Tables 18a, 19a–19d): QPSK 2/9 and
  pi/2-BPSK 1/5, 11/45, 1/3 (medium), 1/5 and 11/45 with spreading factor 2,
  1/5, 4/15, 1/3 (short; "1/5" is S2's short 1/4 code). Shortening (Xs
  zeros, certain at the decoder) and puncturing (every P-th parity bit until
  Xp, erased), as gr-dtv's encoder does.
- **Demod**: anchors the carrier on the extra pilots too; after the frame,
  puts the VL-SNR header back as sent (it is not scrambled), reads it, and
  for pi/2-BPSK takes off the rest of the ±1 scrambling (§5.5.4.1).
- **FEC**: data symbols by layout, LLRs (2-PAM, spread pairs summed; or
  QPSK), shortened and punctured bits restored, LDPC, BCH, BBFRAME (a ragged
  K's spare bits cleared).
- **Modulator**: `FrameSpec::vlsnr(header)` builds them, dummy included.
- **Tests**: every VL-SNR MODCOD and the dummy, between S2 frames, decode at
  8 dB to the BBFRAMEs sent. A sweep (`vlsnr_snr_sweep`, ignored) decodes
  them down to 0 dB; below about −2 dB the PLHEADER is not acquired at all —
  VL-SNR's own territory (−10 dB) needs acquisition built for it (longer
  correlation, header-aided). One seed lost three of six frames at +2 dB
  while 0 dB was clean: worth a look when a real VL-SNR signal turns up.

**S2X left**: superframing (Annex E, M5 in DESIGN), and searching the
preferred PL scrambling sequences of Table 19e when the default does not
lock.

## Demodulator: APSK within 1 dB of the ideal, scrambling found (2026-10-08)

Measuring S2X against EN 302 307-2 Table 20a's ideal Es/N0 showed the FEC
chain itself within 0.5–1 dB on plain AWGN (`awgn_modcod_sweep`, and every
LDPC code's waterfall sits in rate order — `ldpc_waterfalls`), but the full
receiver losing 3 dB and more on 4+12 16APSK and 4+12+20+28 64APSK — and S2's
own 16APSK 2/3 losing two frames in eleven at 12 dB. Whole frames failed, in
runs: carrier slips. Three changes to the carrier loop:

- **Decision error weighted by amplitude** (`Im(y·d*)`, not `arg`): an inner
  ring's angle is several times noisier than the outer's.
- **Slip repair**: each pilot block's anchor reports how far the loop had
  drifted; beyond a quarter of the constellation's finest rotational step,
  the data since the previous known block are re-derotated by interpolating
  between the two blocks' phases, from the raw symbols. On constellations
  denser than QPSK the loop's frequency is reset to the blocks' too (not on
  QPSK: at −1 dB that lost every 13/45 frame).
- **Bandwidth by density**: the decision-directed loop's bandwidth is set for
  the SNR less 20·log10(densest ring / 4) — a 12-point ring decides angles
  three times closer than QPSK.

Now (`s2x_threshold_sweep`, ignored): 8PSK 25/36, 16APSK 26/45, 32APSK 32/45,
64APSK 11/15 and 256APSK 3/4 decode every frame at their ideal + 1 dB; QPSK
13/45 at + 2 dB (below that, PL sync). The GUI scene is unchanged (100 %).

**PL scrambling (Table 19e)**: a frame's pilots descrambled with the right
sequence are one repeated symbol, so differentials across each pilot block
add up coherently (~1) and with a wrong one do not (~0.05) — no carrier lock
needed. When the given gold code does not fit, the demodulator tries the
seven preferred ones (0 and k·10 949) and switches; the side panel shows
"using … (found)".

## RCV-20x modes, part 1 — DVB-S (done, 2026-10-09)

Rory sent the CTCOM RCV-20x manual ("add this as a mod"): its decoder modes,
after S2X, with TPC 2964 (IESS-315) the most important. IESS-315 has to be
fetched first (asked for, not downloaded); DVB-S, also on the RCV-20x's
list, needs nothing new and came first.

- **`decdvb-modem`** (new crate). `conv`: K = 7, G1 = 171₈, G2 = 133₈,
  puncturing for 1/2 … 7/8 (EN 300 421 Table 2), streaming soft Viterbi with
  block traceback — conventions checked against gr-dtv's DVB-T inner coder.
  `rs`: RS over GF(256) (0x11D, roots α⁰…), any shortened (n, k): DVB-S's
  204/188 and the Intelsat sizes the RCV-20x lists. `interleave`: Forney
  I = 12, M = 17. `dvbs`: transmitter, and a blind receiver — every
  (rate, puncturing phase, 90° turn, inversion) is decoded and re-encoded
  over 3000 symbols, each scored against the median of its own rate (a 7/8
  code re-encodes noise almost as well as a right 1/2 does a noisy signal),
  then packet sync on 0x47/0xB8, deinterleaving, RS, energy dispersal (PRBS
  starts 03 F6 08, as it must).
- **Engine**: the FEC thread takes DVB-S symbol blocks as well as DVB-S2
  frames; the TS stage takes whole packets, so DVB-S gets the same outputs,
  analyser, MPE/IP and multicast audio.
- **GUI**: decoder "DVB-S → MPEG-TS" (`--decoder dvbs`), a DVB-S card (code
  rate found, channel BER, RS), the TS card.
- **Scene**: carrier C is now DVB-S, QPSK 3/4. A transmitter's first ~1100
  bytes are its interleaver's zero fill, which made Identify read 6 kS/s:
  the scene and test start mid-stream, as a real carrier is.
- Tests: RS to t errors for five sizes, every rate round trip, every
  rotation/inversion/start, rate 1/2 at 3.5 dB, and a DVB-S VFO end to end.
