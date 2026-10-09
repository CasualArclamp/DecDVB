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

## Inside FastLink: Paradise closed network + ESC (2026-10-09)

The FastLink data rate, 134 925 bit/s, is exactly 21/20 of Rory's
128.5 kbit/s: an overhead octet after every 20 data octets.

- Folded at 672 bits (four overhead octets) the overhead shows: a frame
  alignment word `1x011000` (98h, one bit ESC), an ESC octet, a control
  octet `0xm1xxxx` (`m` a 64-group multiframe pattern, `x` ESC) and another
  ESC octet. 22 ESC bits a group, 0 when idle, in bursts of 38–59 groups
  (short packets; ~4.4 kbit/s against the menu's 4800 baud). No HDLC in
  them under any common descrambler or bit order — unread for now.
- Aligned 99.97 % of 7 minutes (the misses all where the LDPC slipped), and
  found unaided on Rory's second Q-Flex capture: `decdvb-modem::paradise`
  is a payload-search format ("Paradise closed network + ESC framing").
- The 128.5 kbit/s inside: 2 ms frames of 257 bits — one bit of a
  ~500 bit/s side channel (random-looking), then sixteen 16-bit words at
  8 kHz: two 64 kbit/s timeslots. In silence the words repeat every 4 ms
  with a few bits moving; in traffic they fill up. Which bits are which
  timeslot's octet is not settled: neither a byte nor a bit split gives the
  CDM-600L timeslot's 1 ms idle cycle (this mux idles on a 4 ms cycle).

## The Q-Flex's 128 kbit/s: sixteen 8 kbit/s sub-channels (2026-10-09)

Inside the Paradise framing the 128.5 kbit/s are 2 ms frames of 257 bits
(one side-channel bit, then 256), and the 256 are sixteen 16-bit words at
8 kHz. Read bit by bit position, each word is sixteen 8 kbit/s sub-channels
(I.460-style), not two octets:

- Bits 0, 1, 2 and 15 repeat a 160-bit (20 ms) frame in silence — codec
  channels, as on the CDM-600L timeslot (whose 8 kbit/s codec channels idled
  on 80 bits, one with a 0 every 5th bit and a 1 every 160).
- Bits 3–11 and 14 cycle on 4 ms idle patterns; 12 and 13 are always 0.
- Over the 7 minutes no codec channel ever goes active (no speech: squelch
  shut). The only traffic is a burst every 5 s (activity autocorrelation
  peaks at 10 × 0.5 s on bits 3–14), mostly in bits 8–13: no HDLC in it,
  in any grouping or polarity — the voice multiplexer's own status or
  keep-alive, presumably.
- So nothing to hear in this capture, and the codec (8 kbit/s channels,
  20 ms frames) is still unknown: not G.729 in its ITU bit order (tested on
  the CDM-600L's active channels). A capture during a transmission, and the
  multiplexer's make, would be the way on.

## Narrow SCPC carriers: PLL bandwidth, Viterbi decoder (2026-10-09)

Rory's screenshots of five ~10 kBd carriers near 12.3337 GHz showed a ring
for a constellation while the demodulator said "locked, MER 8.8 dB". A ring
scores ~7 dB against QPSK decisions, so it never locked: at Bn·T 0.008 the
carrier loop is ~80 Hz wide at 10 kBd, too narrow for the LNB's phase noise.
A decision-directed loop at 0.02–0.04 gives 15 dB on the same capture. The
PSK demodulator now sets Bn·T = clamp(250 Hz / Rs, 0.008, 0.04) — unchanged
from ~31 kBd up — and the capture demodulates at MER 12.4 dB.

- The carrier: QPSK 10 240 Bd. Not plain QPSK-with-noise but proper QPSK
  (symbol-to-symbol phase steps ¼ 0, ¼ ±π/2, ⅛ each ±π). A window-rank test
  (rows of 40 symbol pairs: rank = 40 + code memory) found memory 6, and the
  null space gave the taps of 133/171 written backwards — the K = 7 code of
  DVB-S and IESS-308/309 (an earlier syndrome check of mine had the window
  reversed). The DVB-S receiver did find rate 1/2 but restarted for want of
  TS sync bytes: hence `ViterbiRx` (dvbs.rs) — the same blind search, no
  transport layer — and the **Viterbi K=7** VFO decoder / `--decoder viterbi`.
- On the capture: rate 1/2, mirrored, channel BER 7 × 10⁻⁴. Under V.35
  (taps 3, 20) the data are 0x55 fill in 16-byte blocks (one varying byte,
  cycling ff e0 00 5c / 00 1f ff a3, then 15 × 55h — 10 240 = 9600 × 16/15,
  EDMAC-like) between half-second bursts of random-looking data. The payload
  search now names a scrambler from idle fill alone (`Format::Idle`: bits
  repeating 1, 2 or 8 back ≥ 75 %, once the framing formats had 8 probes).
- A VFO publishes its status every 250 ms with no input as well: on a short
  file played fast the last publish came before the FEC thread caught up.
- The 128 kBd QPSK carrier next to them (Rory's second capture, MER 9.3 dB):
  no K = 7 rate, no TPC 2964 UW, no repeating unique word in 8 s, not an
  uncoded scrambled stream. Open; a long code (IESS-308 sequential K = 36)
  is not ruled out at that error rate.

## DVB-CID (2026-10-09)

Rory asked for "DVB-CI": the carrier ID (ETSI TS 103 129 V1.1.1, fetched
with his go-ahead, cache deleted), not the common interface.

- `decdvb-modem::cid` follows §4–5: the 244-bit frame (UW 147147h,
  complemented every other frame; two halves of identifier, content ID,
  24 information bits, CRC-8, BCH(111, 69)), the x⁹ + x⁵ + 1 scrambler, four
  copies, differential encoding, the 4096-chip x¹⁵ + x¹⁴ + 1 sequence,
  112/224 kchip/s, +220 Hz. Checked against the document's own examples:
  the first 32 chips (5091E364h), the check digits of 00:06:B0:FF:FF:01:AC:07
  (75h), the position and telephone codings. The BCH generator is the
  product of table 4's six polynomials; with GF(2⁷) from x⁷ + x³ + 1 its
  roots include β¹…β¹² for β = α²³, so Berlekamp–Massey corrects 6.
- The scrambler figure can be read two ways (register x⁹…x¹ written left to
  right or right to left); the receiver tries both, keeps the one that
  passes the CRC and says so.
- Receiver: VFO baseband → DDC onto the host's centre (Identify) → cubic
  resampling to 4 samples a chip → FFT search over code phase × frequency
  (±1.7 kHz, half-bin steps, 24 bits non-coherently) → per-bit prompt,
  early and late correlators with an FLL on the squared differential →
  DBPSK → UW four times 244 apart → the copies summed → descramble, BCH,
  CRC → identifier, MAC, position, telephone, text (§4.2 table 1).
- Tested on synthetic signals: a full frame decoded from mid-stream at a
  chip SNR of −26 dB; through a VFO, a CID 27.5 dB under a 224 kBd QPSK
  host (Es/N0 15 dB) found within 10 Hz of +220 Hz. None of Rory's
  recordings (CDM-600L, Q-Flex ×2, three others) carries a CID: no code
  correlation peak in any of them.

## FFT size without a restart (2026-10-09)

The waterfall's FFT size used to need **⟳ Apply** on a file and a restart
of the radio. It is now an engine command (`Engine::set_fft_size`, like DC
removal): the front-end thread swaps its spectrum and block length between
blocks, drops rows of the old width and starts the smoothed spectrum again;
the source, the HackRF and the VFOs carry on untouched.

## Station names from SAP, and an SDP that names the wrong payload (2026-10-09)

A live DVB-S2 radio multiplex announces each station by SAP (224.2.127.254)
with SDP lines ended by a bare CR; the parser now splits on CR or LF, so the
`s=` names show (and bare SDP sent to any port counts as an announcement;
a stream takes the announcement for its group and port, else the only one
for its group, else the only one for its port). The same SDPs say
`m=audio … RTP/AVP 14` (MPEG audio) while the packets are payload type 99:
ADTS AAC behind an RTP header extension. Trusting the SDP made the player
strip RFC 2250's 4 bytes from every packet and look for MPEG audio, so the
named stations went silent. An announcement now describes a stream only for
the payload type the packets carry (`SdpInfo::for_pt`): otherwise it names
the stream and the codec comes from the bytes, and the external-player relay
serves the ADTS over HTTP rather than handing over the misleading SDP.

## DVB-CID: the code search on screen (2026-10-09)

`CidRx` keeps a map of every acquisition search (`CidStats::search`, an
`Arc<CidSearch>` so the stats copy cheaply): the correlation over all 4096
code phases at the strongest frequency (512 points, the largest of each 8
chips), and code phase ± 24 chips × the whole ±1.7 kHz searched (pooled to
at most 64 rows), all in dB over the mean of every cell, with the peak and
the lock threshold (4.8 dB). The Carrier ID card draws both under "Code
search": while searching it shows how close the best cell came; once locked,
the search that found the code (a single spike, and one bright cell near
+220 Hz). Checked on a simulated 1.008 MBd QPSK host with a CID 27.5 dB
under it: peak 6.6 dB, found at code phase 1299. The frequency reads from
Identify's estimate of the host's centre, here 310 Hz high, so the CID shows
at −80 Hz rather than +220.

## Test signals in tests/; a DVB-CID generator (2026-10-09)

`decdvb cid` writes a DVB-S2 QPSK 1/2 carrier (1.12 MBd, α 0.20, the test
TS) with a DVB-CID under it at 224 kchip/s, −27.5 dB, +220 Hz; 3.36 MS/s so
both are whole samples (3 a symbol, 15 a chip). The CID frames cycle the
fields set by the new `CidReport::set_position / set_telephone /
set_user_text` (inverses of the decoders; §4.2's example bits come out
exactly) two to a frame (`frame_fields`), the UW complemented on odd frames;
the capture opens a second before a frame so the receiver is locked when it
starts. 30 s holds one whole frame (identifier, position); all six take
108 s. `decdvb test-signals [dir]` writes the scene and the CID capture into
`tests/` (git keeps only its README). Checked with the CLI: the CID decodes
(identifier, MAC, position), Identify calls the host DVB-S2 QPSK 1/2 with
pilots, and its TS decodes. The CID's offset reads +523 Hz: Identify's
centre for the host is ~300 Hz low, as on the GUI test (−80 Hz there).

## DVB-CID live view; three receiver fixes (2026-10-09)

`CidStats::live` (`CidLive`) records the tracking a bit at a time: the
prompt correlations turned onto the real axis (the bits' squares averaged
give the phase) at unit power, the differential products, SNR and
frequency over 256 bits, early/prompt/late magnitudes averaged over ~10 bits,
the last 244 soft bits, and the frame sync (copies of the UW in a row, time
to the frame being whole). The Carrier ID card draws them as instrument
panels (painted, not egui_plot): two constellations with MER, the E·P·L
stems on the ideal correlation triangle with (E−L)/(E+L), two strip charts,
a soft-bit bar code and four "copy" boxes filling.

Building it showed the GUI's CID at 0.9 dB a bit where the CLI had 8.4 on
the same file. Fixed:
- Lost lock was "64 bits in a row with |P|² < 2·noise"; off the code one
  bit's power exceeds that one time in seven, so a slipped tracker followed
  noise for good. Now the smoothed SNR under 3 dB for 64 bits.
- Dropped samples (the VFO's queue, or the CID thread's own) are reported as
  gaps (`VfoHandle` counts the samples, `CidWorker::lost`, a `Gap` message)
  and `CidRx::gap` stands zeros in, so timing and phase carry on and only
  the bits in the gap are hurt.
- The frame search waited for a frame + 22 bits, so a frame ending near the
  end of a recording never decoded.
Tests: a reported 60 000-sample gap rides through (one search, the frame
decodes); unreported, the receiver searches again and relocks. In the GUI
the 30 s test capture now locks at 9.3 dB a bit and decodes its frame.

## The Q-Flex's 128.5 kbit/s is a 257-bit TDM multiplex (2026-10-09)

The alignment bit of each 257-bit frame (2 ms; found by its column making
the rest 16-periodic) is now read: a 70-frame pattern, 99.9 % stable — on
alternate frames the Barker-7 sequence reversed, 0100111, again and again
(a 14-frame, 28 ms cycle), and between them "01101" repeating (one bit every
4 ms, so a 20 ms cycle: the codec frames' marker). So the 128.5 kbit/s on the
modem's data port is a multiplexer's own TDM stream: 500 bit/s of framing
and 128 kbit/s of payload as sixteen 16-bit words at 8 kHz (two 64 kbit/s
timeslots' worth). Its make is still unknown.

What 7 minutes show, channel by channel (bit n of each word, 8 kbit/s):
- 0, 1, 2: idle codec — one 160-bit frame repeated (20 ms); laid out in
  octets each frame is five 4 ms sub-frames whose first octet is a
  per-channel constant (11000000, 10000100, 10010000), every octet starting
  with a 1.
- 3, 4, 5: a fixed 4 ms pattern.
- 6–15: varying, more for about 20 s in every 60 s. Bits 8–14 hold one
  7-bit value per 2 ms, changing at word 10 of every frame (P = 0.999), in
  one of eight states (66/0/1, 64/32/33, 16/96/97, 3/4/5, 48/72/73, 9/6/7,
  12/18/19, 36/24/25) that steps every 0.5–1.5 s: signalling, not audio.
  (Corrects the earlier notes: bits 12 and 13 are not always 0, and the
  activity is a 60 s cycle, not a 5 s burst.)
- No channel carries speech in this capture.

ESC: the bursts (0.2–0.3 s, on multiframe boundaries) are neither async
characters (stop bits fail as often as random) nor HDLC, plain or under
the usual descramblers: scrambled or encrypted binary, presumably the modems'
M&C.

`decdvb-modem::tdm257::TdmRx` finds the frame by the alignment word (every
phase × parity × rotation, ≥ 95 % of 56 word bits), checks it every other
frame (5 wrong of the last 14 → search again), and meters each channel over
0.5 s: bits changed since 20 ms and since 4 ms, and ones → fixed, 4 ms
pattern, idle codec, varying, or active (> 25 % changed at 20 ms). It runs
on Paradise data in the FEC thread (`FecStats::tdm`); the CLI prints the
channels, the FastLink card draws them as bars (speech should stand up near
half). On the 7-minute capture: aligned, 214 030 frames, 5 word-bit errors,
1 loss (at the FastLink slip). Next: a capture while someone talks, and the
multiplexer's make — then the codec.

## IESS-308/309 narrow carrier: differential decoding, IBS framing, 90° turns (2026-10-09)

Rory's chain for the 10.24 kBd carrier: PSK ← K=7 rate 1/2 ← differential
encoding ← IESS-308 scrambling (taps 3, 20) ← data + IBS/SMS framing. The
"0x55 fill in 16-byte blocks" found before was the missing differential
decoding: with it the data are an idle line's all ones, and the 0x55/0xAA
blocks (whole frames inverted four at a time) go away. Differential decoding
and a self-synchronising descrambler commute.
- IBS (`decdvb-modem::ibs`): 128-bit frames, one overhead octet then 120
  data bits (16/15), the overhead in a four-frame cycle — on this carrier
  00 20 00 E4, with 10h of the second toggling (a service bit). IESS-309
  was not to hand, so the overhead is learned, not decoded: the receiver
  takes the octet window whose bits each keep a four-frame cycle (all but
  two), changing most across the cycle, then most zeros (idle all-ones data
  repeat too); loses it at 10 of 16 frames misaligned. Scored on data bits
  only, so an E1 (whose alignment octets also make a 4-frame cycle at
  128-bit spacing) still wins as E1.
- The payload search tries every descrambler with and without differential
  decoding (plain wins ties); `PayloadStats::ibs`; CLI and GUI show the
  overhead cycle and service bits.
- The "data bursts" were not data: the carrier turns 90° for ~0.4 s, three
  times in 6 s; decoded under the turned orientation the bursts are the same
  idle IBS frames. `ViterbiRx` now checks the fit every 256 symbols (over
  the last 384) and, when it fails, follows the orientation that fits at
  least twice as well — rotation and mirroring only, the trellis not reset,
  so no bit is lost or added (`ViterbiStats::turns`). Test: a 90° turn for
  4000 symbols, followed twice, the bits still aligned at the end.
- On the 6 s capture: "IBS/SMS framing (IESS-309, 16/15), differential
  decoding, self-synchronising descrambler, taps 3, 20"; 3 turns followed,
  channel BER 0; IBS aligned, 381 frames, 1 loss (before the first turn was
  followed). The data are idle throughout.

## DVB-CID: the right centre, deeper searches, a frequency-loop false lock (2026-10-09)

Rory asked to lower the lock threshold: a live 1.048 MBd DVB-S2X carrier's
search peaked at 3.9 dB against 4.8. That is the noise: ~1 M cells of a
24-bit sum (Gamma(24) over its mean) top out near 3.8 dB, so a lower fixed
threshold would lock on noise. What was wrong:
- The search was centred on Identify's centre; that carrier's residual
  offset (from the carrier lock) was −8.6 kHz, so a CID 220 Hz from the true
  centre lay outside the ±1.7 kHz searched. The CID VFO now centres on
  Identify's centre plus the residual when the carrier locked, and searches
  ±4 kHz otherwise (`CidRx::with_span`).
- Searches deepen: 24 bits, then after three empty searches 48, then 96.
  The threshold is computed for the bits and cells (Chernoff bound on the
  largest of the cells, one false lock in 10⁴ searches): 4.9, 3.7, 2.7 dB
  (`CidSearch::bits` shows the depth).
- Moving the centre exposed a tracking bug: the frequency loop squared the
  bit-to-bit phase step, which also locks half a cycle a bit away (±27 Hz
  at 224 kchip/s): 3.9 dB lost and every bit inverted, so no frame (8.4 →
  4.7 dB on the test capture). It now resolves that ambiguity with the phase
  between the two halves of a bit (same data bit, unambiguous within ±1 bit
  rate) and tracks with the quiet squared detector. Test: locks true at
  +20, +97, +300, −150 Hz (+20 false-locked before).

## Generic PSK: symbol numbering in the .bin (2026-10-09)

`VfoSettings::symbol_labels` (`psk::SymbolLabels`): the byte written per
symbol is the standard label (as before: DVB-S2's mappings, the RCV-20x
manual's for its QAMs), the point's position (PSK: by angle from the first
point counter-clockwise of 0°; square QAM: x index then y index from the
lowest), or the Gray code of the position (per axis for QAM; neighbours
differ in one bit). APSK and 8QAM keep their labels. GUI: "Numbering" under
the PSK constellation; CLI: `decode --decoder psk --labels natural|gray`.
Checked on the 10 kBd QPSK capture: Gray is a fixed relabelling of the
standard labels (0→0, 1→2, 2→1, 3→3).

## Q-Flex: record on activity; DVB-CID: weak ones (2026-10-09)

Q-Flex: the second capture (QFLEX_2, 14 s) is the same — codec channels 0–2
idle, no speech — but its alignment channel's 20 ms word reads 11110 where
qflex_3's read 01101: it carries state, not a fixed marker. With no speech
on record, the way on is to catch some: the FastLink VFO has "Auto-record:
when a TDM channel goes active" (`VfoSettings::record_on_activity`). The FEC
thread keeps the last 10 s of data while not recording, starts a
`…-fastlink-active-….bin` with that pre-trigger when any channel goes
Active, and stops 10 s after the last activity (`FecStats::raw_triggered`
counts them).

DVB-CID: on Rory's live 16APSK host the strongest cell of a 96-bit search
(2.2 dB, under the 2.7 dB threshold) sat at −3418 Hz while Identify's
(unlocked, power-line) residual put the carrier at −3621 Hz — 203 Hz apart,
where a CID belongs; under a 1 % chance for noise in an 8.4 kHz span. So a
real CID ~10 dB under the specified level. For those:
- a 192-bit search level (threshold 1.9 dB);
- unlocked hosts searched wide enough to take in Identify's residual too;
- found at 96 bits or deeper, tracking is gentler (FLL gain 0.01, DLL 0.05)
  and keeps lock down to a 1 dB power/noise ratio (a leaky counter: below
  counts up, above counts down twice as fast);
- the acquisition's frequency is interpolated between half-bin cells;
- the unique word is looked for on the four copies summed (6 dB better; a
  chance match fails the CRC).
Test (ignored, `--release`): a CID 41 dB under the noise (~1.4 dB a bit)
is found by a deeper search, tracked, and its frame decoded.

## DVB-CID low-SNR mode (2026-10-09)

A "Low-SNR mode" button on the Carrier ID VFO (`VfoSettings::cid_low_snr`,
CLI `--low-snr`), taken up by the CID thread live: searches start at 96
bits and go to 384 (7 s at 224 kchip/s), with a false lock allowed once in
100 searches (thresholds 2.6, 1.7, 1.3 dB); tracking is the deep kind
throughout. Searches now run on up to 8 cores (each bit's spectrum once,
the cells split across threads). Deep tracking gained:
- a Costas loop after a 200-bit frequency pull-in (wide ~4 Hz for 300 bits,
  then ~1.5 Hz), so bits are detected coherently;
- the acquisition's code phase interpolated to a quarter chip, and the
  timing loop quick for 200 bits then slow;
- in low-SNR mode the frame's four copies combined coherently (each copy's
  sign from correlating whole copies) before the differential decoding,
  the summed-differential way as fallback.
Measured (simulation, CID at x dB under unit noise a sample, 4 per chip):
normal mode locks to −42..−43 dB; low-SNR mode to −46 dB (−45: normal
cannot, low can — test); frames decode to about −42 dB (~1 dB a bit) in
either. Tracking shows ~0.6–1.3 dB less than ideal: the 4-per-chip grid
(chip edges between samples) and timing dither — 8 per chip would halve it.

## DVB-CID at eight samples a chip (2026-10-09)

`cid::SPS` 4 → 8 (the CID thread resamples to 8 × the chip rate): a chip's
edge now falls within 1/16 chip of a sample. Early/late stay a quarter chip
out (±2 samples), timing steps an eighth, the timing loop's gain scaled to
move as fast in chips; the search decimates by SPS/2 to two a chip (its
frequency bins follow: a first try took them from fs/2 and doubled every
acquisition frequency). Weak-signal tests now set the level as SNR a bit.
Measured: tracking ~0.6 dB short of ideal (was 1.3–1.6); frames decode to
−0.5 dB a bit in low-SNR mode, 0 dB in normal (was about +0.1).

## DVB-CID on a live carrier: phase noise and clock error (2026-10-09)

Rory's 3.58 MBd 8PSK host: the CID found at 9.2 dB, tracked at 8.4 dB a
bit, but the despread bits a ring and no unique word, in low-SNR mode.
Simulated with real-world impairments (8 dB a bit):
- Phase noise (random walk, 0.6 rad a bit): low-SNR mode looked for the
  unique word on the coherent combination only, which phase noise defeats.
  Now either the coherent or the summed-differential combination may find
  it, and either decoding is tried; the Costas loop has a lock detector
  (cos 2φ averaged) and is given up below 0.3, leaving the frequency loop.
- Clock error (the receiver's chip clock a few ppm out — a HackRF is
  ±20 ppm): the timing loop was first order and lagged; at 5 ppm neither
  mode decoded. It is now second order (an integrator for the drift, the
  discriminator in samples: 4x/(3·SPS)), the deep gains scaled with the
  SNR. The integrator gives the clock error (`CidStats::clock_ppm`, shown
  in the card).
Now: both modes read frames with 0.3–1 rad of phase noise, and with 5 ppm
plus 0.6 rad together (test); normal mode copes with ±12 ppm, low-SNR mode
±8 (its 96–384-bit searches smear the code peak beyond that; a clock
correction would extend it).

## Clock correction (2026-10-09)

One value per receiver (`VfoSettings::clock_ppm`, the GUI keeps every VFO
on the one in prefs.txt, `clock_ppm=`; CLI `--clock-ppm`): how much faster
transmitters' clocks run against the SDR's, in the same terms the CID card
measures. The CID thread resamples to the chip rate as our clock sees it
(SPS × rate × (1 + ppm·10⁻⁶), retuned live), so even 384-bit searches see
no drift. The Carrier ID VFO's settings show it with "Use measured", which
adds the timing loop's measurement. Checked: the CID test capture decoded
as if 15 ppm out (rate given 15 ppm high) — uncorrected, neither mode reads
a frame (8–16 searches); with --clock-ppm 15 both lock at once, 9.9 dB a
bit, identifier read.
