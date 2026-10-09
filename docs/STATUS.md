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

## RCV-20x modes, part 2 — turbo product code machinery (2026-10-09)

`decdvb-modem::tpc`: extended Hamming codes (any m, shortened), their
two-dimensional products, and the iterative Chase–Pyndiah soft-in/soft-out
decoder (16 test patterns, Pyndiah's α/β schedule). On the shape of IESS-315's
`tpc_2964` — (64,57) × (46,39), 2223 data bits in 2944, rate 0.755 — a
frame with ~100 raw errors (Eb/N0 3.5 dB) decodes in 9 or more of 10.

**Not done, and why:** what makes it *tpc_2964* rather than a product code
of that shape — where the 20-bit unique word F50B8h sits, the bit order of
the 46 × 64 block, the Hamming polynomial (x⁶ + x + 1 is assumed only for
the tests), the (2,3,9,12) descrambler with preset 475h and where it runs
— is in IESS-315 (or could be found from a real carrier with the parity
checks as the judge). Waiting on Rory for the spec or a capture.

## RCV-20x modes, part 3 — QAM in the generic decoder (2026-10-09)

8QAM, 16QAM and 64QAM join BPSK…32APSK in "Generic PSK → symbols", labelled
as the RCV-20x manual's Figures 3.2/3.3 label its hard decisions (16QAM is
natural binary per axis there; 64QAM Gray). Identify reads rings, not
grids, so a QAM carrier is named in the VFO's modulation setting.

Decision-directed carrier recovery false-locked 16QAM 27° off (MER 11 dB):
the generic demodulator now normalises symbol power itself (the AGC levels
samples, not symbols), acquires multi-ring constellations on the outer ring
alone (the reduced-constellation algorithm), and takes full decisions only
above 0.7 coherence (back below 0.4); a false lock still scores ~0.45, so
multi-ring "locked" needs 0.6. 16QAM: MER 34 dB on a clean carrier.

Left out: OS8QAM, the RCV's 8APSK and 32QAM (ring ratios / geometry not in
the figures), OQPSK and pi/4-DQPSK (they need demodulator changes).

## RCV-20x modes, part 4 — TPC 2964 (2026-10-09)

Rory: "download IESS-315 and do TPC 2964". IESS-315 (rev. E, 2005) turned
out to be a performance document — EIRP, link budgets, a spectrum mask, a
data scrambler given only as figures — that leaves the turbo code to
"compatible turbo modems", and it carries an Intelsat confidentiality notice:
nothing from it is in the repo, and nothing was needed from it. What is
used comes from the RCV-20x manual (Table 3.2) and the signal.

- **`decdvb-modem::tpc2964`**: UW F50B8h every 2964 bits found under each
  phase ambiguity (2 for BPSK, 8 for QPSK), three frames running. The frame
  structure is searched: send order (rows or columns), either direction in
  each, the Hamming generator (six primitive polynomials), the code bits
  unscrambled or scrambled by the (2,3,9,12)/475h sequence wired four ways
  and started at the block or the UW — 432 hypotheses. Each is scored by
  min(rows, columns) that are codewords as received; a column-major block is
  the transpose of a row-major one, so only the 46 × (64,57) orientation is
  kept (the first version counted each layout twice and could not choose).
  Near-equivalents (a reversed Hamming word is a word of the reciprocal
  polynomial's code, so some fit a quarter as well) are settled by soft
  decoding. Slips: when the UW is missing where due, the other orientations
  and ±2 symbols are tried there and taken if this UW and the next agree.
  Frames that missed their UW and do not decode are dropped.
- **`decdvb-modem::payload`**: HDLC deframing (flags, zero-bit stuffing,
  FCS-16/32), TS alignment at any bit offset (three syncs to lock), and the
  descrambler search: none, self-synchronising (2,3,9,12), its reciprocal,
  V.35 (3,20), V.29 (18,23), and additive (2,3,9,12) per frame four ways.
  Random data never passes (tested).
- **Engine/GUI**: decoder "TPC 2964 (IESS-315) → IP / TS" (`--decoder tpc`),
  BPSK/QPSK (or Identify's), a TPC card (UW, orientation, structure and fit,
  decoding, payload, data file); IP from HDLC goes to the IP stage (PCAP,
  flows, multicast audio), TS to the TS stage; Record writes the data `.bin`
  as well. Identify names the carrier from its UW.
- **Scene**: carrier D, TPC 2964 QPSK at 125 kBd, +250 kHz, a multicast radio
  and unicast traffic over Cisco HDLC, self-synchronising (2,3,9,12)
  scrambling. Found, decoded and the radio listed in the GUI.
- **Not covered**: 8PSK and 16QAM tpc_2964 (labels and soft demapping still to
  do), OQPSK (IESS-315 allows it; needs the demodulator), a scrambler running
  across frames, the V.35 run counter. A real TPC 2964 recording would
  confirm which hypotheses are real; until then it is checked on synthetic
  signals only.
- Also this session: **v0.1.0 released** — `release.yml` (DecDRM's pattern)
  builds the static-CRT exes on a tag, checks versions and the C runtime,
  smoke-tests the CLI and drafts the release; `decdvb-gui --version`.

## Proprietary-waveform fingerprints, live text (2026-10-09)

Rory: "look for modes like NovelSat NS4 and Q-Flex FastLink", and "in the
generic, something for looking for live strings". Also **v0.2.0 released**
(TPC 2964).

What is public (vendor sheets, manuals, a NovelSat patent; summarised from
reading, nothing kept in the repo): NS3/NS4 use BCH + LDPC with 64800/16200
frames, extra code rates (13/30, 7/15, 8/15, 17/30, 19/30), roll-offs down
to 2 % (NS4), a "Golden-Sequence" setting 0–262141 (the S2 Gold-code range)
and scrambling "reset at the start of a frame" sparing the header — S2-style
framing is likely but the PLS meaning is not published. Paradise FastLink
(Q-Flex) publishes rates and roll-offs only; its "DVB-S2X low-latency"
extension adds 5400- and 3240-bit frames. Comtech VersaFEC-2: 9600/1600-symbol
blocks. None publishes a sync word.

- **Identify**: `Dvbs2Info::unconfirmed_spacing` — S2 headers recurring on a
  grid their PLS codes do not explain → "DVB-S2-style PLHEADERs every N
  symbols … a proprietary DVB-S2 extension (NovelSat NS3/NS4, or a vendor's
  short frames)". `roll_off_2pct` — fits 2 % better than 5 % → "sharper than
  DVB-S2X allows". The roll-off fit held its floor at −300 dB when the fitted
  floor went negative, which made a 2 % carrier read 35 %; now it is held at
  the measured floor.
- **`period.rs`**: frame-structure finder — autocorrelation of unit-magnitude
  locked symbols by FFT; the first peak at least half the strongest, ≥ 6×
  the median; with ≥ 8 frames, frames folded (pass 1 straight, pass 2 each
  frame turned onto the candidate positions) to find the runs of repeating
  symbols. A period with nothing repeating in the fold is the data's own
  regularity (unscrambled text) and is not reported. On the scene's DVB-S
  carrier it finds the synthetic TS's 40-packet repeat.
- **Live text** (`decdvb-modem::text`, `psk::TextSearch`): generic PSK VFOs
  read the decided bits 32 ways per carrier orientation (plain/differential
  × 8 alignments × 2 bit orders), keep runs of ≥ 6 printable characters, and
  call the reading with ≥ 4× the median's text the one; strings ≥ 10 shown
  live, the longest from any reading as candidates. 3.6 Msym/s for QPSK.
- **Spectral lines**: unscrambled data puts lines on its carrier, which made
  the ENBW (and so the symbol rate) collapse and the carrier read as a lump.
  `estimate::flatten_lines` takes up to 8 narrow lines standing on a plateau
  out of the level and flatness measures (not a CW on the floor, not a comb,
  not lines holding most of the power), and the ENBW reference is now also
  bounded by the median over the middle half of the occupied band.
- Not possible without captures: naming FastLink/VersaFEC; confirming the NS3/NS4
  header layout. HackRF tops out at 20 MS/s, below most NS4 hypermuxes.

## Text in every decoder (2026-10-09)

Rory: "add the strings decoding to all decoders". **v0.3.0 released** before
it (live text in the generic decoder, the proprietary-waveform cues).

- `decdvb-modem::text::ByteText`: runs of ≥ 6 printable bytes, counted; the
  strings seen more than once are kept in their own small table, so ranking
  costs nothing and the view is built on every frame (a cached view went
  stale as soon as data stopped — the test saw 140 of 17 000 bytes); up to
  65 536 distinct strings remembered, single sightings dropped oldest first;
  strings ≥ 16 kept as they come. `push_keyed` carries a run on per stream.
- TS stage: payloads after the header and adaptation field, per PID (a
  table's strings span packets). IP stage: UDP/TCP payload (IP header and
  L4 header skipped). TPC 2964 with an unknown payload: `TextFinder` over the
  data bits (32 readings). Identify's live view: `TextSearch` as the generic
  decoder (its "look for text" setting now shows for Identify too).
- GUI: "Text in the transport stream" / "Text in the IP packets" (recurring
  with counts, latest long strings); the bit-level card for TPC-unknown and
  Identify.
- Tests: the DVB-S VFO finds "DecDVB test signal" recurring in its TS; the
  MPE carrier shows the SAP announcement's SDP lines in its IP text.

## E1 voice in a modem's data (2026-10-09)

Rory carries an E1 with G.711 airband voice over a Comtech CDM-600 link.
`decdvb-modem::e1`: G.704 frame alignment at any bit offset (FAS x0011011
every 512 bits with the NFAS bit between, 8 in a row to align, 3 bad to lose
— G.706), timeslots out, CAS seen in TS16, G.711 A-law both ways. The payload
search tries E1 beside HDLC and TS under each descrambler. The FEC thread's
E1 stage meters every timeslot (dBFS over half a second: idle ≈ −70, voice
in between, data ≈ −5) and plays one / records one to `.wav` by handing
20 ms RTP PCMA packets to the multicast-audio player and recorder. GUI: an
E1 card with a level bar, ▶ and ● per timeslot. Test: a scrambled E1 in TPC
2964 with a tone in TS 7 — found, metered, played, recorded. Drop-and-insert
(IBS-framed n×64k) is not handled yet: waiting on what the CDM-600 sends.

## First live Comtech carriers: CDM-600L TPC 2964 and D&I++ voice (2026-10-09)

Rory recorded his own carriers on Horizons 3e (169°E): a CDM-600L and a
Q-Flex.

- **CDM-600L** (43.6 kS/s QPSK): Identify named it from the TPC 2964 UW, and
  the TPC decoder took it: UW every 2964 bits, structure row by row,
  x⁶ + x + 1, code bits unscrambled (the search's own finding, fit 10 % at a
  raw BER of 0.6 %), 426 of 443 frames decoded.
- The payload: data rate 65 423 bit/s = 64 000 × 46/45 — Comtech **D&I++**
  (CDM-625 manual: 2944-bit frames, 64 overhead + 2880 data). Under the
  V.35 descrambler (taps 3, 20) the data fold at 2944 bits into a 24-bit
  header `000001010111101000111000` and four 10-bit overheads
  (`0111111111` ×3, the last with a varying bit) between five 576-bit blocks;
  the data are 360 bytes (45 ms) of the timeslot a frame. (First read as
  open-squelch hiss and hum; wrong — see the next section.)
  `decdvb-modem::dandi` deframes it; the payload search tries it; the FEC
  thread works out the timeslots from the symbol rate (n × 64k × 46/45) and
  feeds them to the E1 voice stage; the GUI's Voice card shows "ch 1..n".
- **False MPEG-TS**: three sync bytes 188 apart turn up by chance in a long
  probe, so the CDM-600L read "MPEG-TS" (5–10 packets, random PIDs). TS, E1
  and D&I++ must now account for at least half of the probed bits.
- `decdvb decode <capture> --decoder … [--out d --e1-record n]`.
- **Q-Flex** capture (95 kS/s QPSK): a structure repeating every 23 076
  symbols (~10 %), no header DecDVB knows — FastLink or Paradise TPC; open.
- With several D&I++ timeslots the byte order (alternating) is assumed, not
  confirmed.

## The CDM-600L's timeslot is not G.711 (2026-10-09)

Rory: the Comtech voice sounds like digital noise. Right — the D&I++
deframing is sound (header and overheads in place, frames every 2944 bits),
but the timeslot does not hold G.711:

- Each octet's bits 1 and 4–8 (G.704 numbering, bit 1 first) repeat a fixed
  pattern every millisecond — bit 1 reads `01100000` over eight octets, and
  bits 4–8 read serially are `0x60` repeated — through the whole capture.
- Only bits 2 and 3 change: two 8 kbit/s streams (I.460-style sub-rate).
  During quiet stretches each repeats an identical 80-bit (10 ms) pattern —
  what a speech codec makes of digital silence; otherwise they look random.
  The bit-2 stream also carries a framing bit every 5th bit (0, with a 1
  every 160 bits = 20 ms); the bit-3 stream has no fixed bits at all.
- Not G.729 in its ITU bit order: the pitch-parity bit (P0 over P1's six
  MSBs) fits at no offset, nor does any 6-bit parity elsewhere in the frame.
  The codec, and the equipment feeding the E1, are unknown.
- So the A-law levels (−9 dBFS, "loud") and the noise heard were the
  idle-pattern bits. The E1 stage now judges each timeslot by which bits
  change (compared with the same bit a millisecond earlier): sign bit
  moving → G.711; nothing moving → steady (idle or a tone); sign bit frozen
  while others move → **not G.711, bits …** (Voice card, `decdvb decode`).

## Q-Flex FastLink, blind (2026-10-09)

Rory sent his Q-Flex's settings (FastLink, QPSK, 0.710, 128.5 kbit/s,
closed network + ESC, Drop-Insert TS1, 95 017 sym/s) and a 6.9-minute
capture. Demodulated losslessly (file playback now waits for slow VFOs and
FEC threads instead of dropping when not real time — `decdvb decode --fast`
keeps every symbol, 18× real time).

- Frame: 18-symbol sync word every 11 538 symbols (3527 found, slip-free),
  then 23 040 bits = four 5760-bit slots.
- GF(2) rank of thousands of slots (and their differences): 4110 / 4109 once
  the ~8 % with bit errors are weeded out by checks from the other half —
  a **(5760, 4096)-ish LDPC code** with a fixed scrambling offset on the code
  bits; 13 positions per slot lie in no check (outside the code), 2 are
  always 0. 4 × 4096 / 23 076 = 0.7100, the menu's rate.
- Sparse checks by random-permutation RREF of the dual: 1251 of weight 16,
  ~180 heavier (24–28): an irregular LDPC; bit degrees spread evenly along
  the slot, so the coded bits are interleaved.
- **Identify** names the framing (`decdvb-modem::fastlink`: sync word under
  any QPSK orientation, three frames running). Reading the data still needs
  the data positions, their order and the data scrambler; the recovered
  checks are kept out of the repo for now.
- Refined: the checks of one 2880-bit half of a 5760-bit slot map onto the
  other's (shift by 2880: 269 of 300) — the code is **(2880, 2048)**, eight
  codewords a frame (2048/2880 = 0.711). Its order sent shows no
  block-cyclic structure, nor after undoing any row–column interleaver of
  2880; bit degrees vary with position mod 16. Next: the interleaver, the
  data positions and the data scrambler — or a capture with the Q-Flex
  sending a test pattern, which would give them directly.

## FastLink decoded: the code, the layout and the scrambler (2026-10-09)

Blind, from Rory's 6.9-minute Q-Flex capture (FastLink QPSK 0.710):

- One codeword at a time this time (2880 bits, slot-wise differences with
  bad words weeded out): rank exactly 2048, a dual of 832, every position
  covered — the "13 positions in no check" were bit errors.
- Random information-set reductions of that dual, a single codeword's,
  converge in seconds: 640 checks of weight 10, all independent, pairwise
  overlaps ≤ 2, in 10 classes of exactly 64; then 192 more of weight 18
  outside their span. Bit degrees: 832 of 2, 2048 of 4 — an **irregular
  repeat–accumulate LDPC**. The degree-2 bits chain all 832 checks into one
  ring (an accumulator closing on a bit that is always 0); each data bit's
  four checks sit at `x + 13·t` on the ring (13 = 832 / 64): 32 circulants
  of 64, a DVB-S2-style address table (EN 302 307-1 §5.3.2).
- Layout as sent: 416 units of (2 parity, 4 data) bits, then 384 data bits;
  data rows of 32 (one bit per circulant), row `r` holding circulant row
  `37·r mod 64`. A 32 × 4 table plus these rules rebuilds the measured
  832 × 2880 matrix exactly. Every check has odd parity as received.
- The data's fixed offset per frame position, read in the order sent, has
  linear complexity 32 across all eight codewords: a frame-synchronous
  scrambler `s[i] = s[i−2] ⊕ s[i−16] ⊕ s[i−18] ⊕ s[i−30] ⊕ s[i−32]` from
  `0xAAA2A2A6` (two interleaved `x¹⁵ + x⁸ + 1` sequences, the even one
  inverted). The data bits are therefore sent in their own order.
- `decdvb-modem::fastlink` decodes it (layered normalised min-sum, odd
  checks, slips of the timing or the carrier phase repaired), and the
  **Q-Flex FastLink** VFO decoder / `decdvb decode --decoder fastlink` hands
  the descrambled data to the payload search, text finder and data file.
  Rory's capture: 783 of 784 codewords decode, channel BER 1.6 × 10⁻⁵.
- What the data carry is next. They hold idle patterns much like the
  CDM-600L timeslot's (1/3 ones, period-8 runs drifting slowly — the
  128.5 kbit/s data rate is 257/256 of 128 kbit/s, so an overhead bit every
  257 is likely); no HDLC.

## FFT size without a restart (2026-10-09)

The waterfall's FFT size used to need **⟳ Apply** on a file and a restart
of the radio. It is now an engine command (`Engine::set_fft_size`, like DC
removal): the front-end thread swaps its spectrum and block length between
blocks, drops rows of the old width and starts the smoothed spectrum again;
the source, the HackRF and the VFOs carry on untouched.
