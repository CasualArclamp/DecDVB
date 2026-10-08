# DecDVB

A **DVB-S2 / DVB-S2X** receiver in Rust, for a **HackRF One** or recorded IQ,
built like **SDR++**: a big waterfall over the whole span, and VFOs you drop on
it, each running the decoder of your choice — including a blind **"what is
this?"** mode that finds a carrier's symbol rate and tells you what it is.

Planned: adaptive coding and modulation (**ACM**) decoding, **GSE** to **IP**
written as a PCAP, **MPEG-TS** extraction, and a matching modulator.

![DecDVB: carriers in an 8 MS/s span, each claimed by a DVB-S2 VFO; the selected one is an ACM carrier, frame-locked, its 16APSK frames carrier-locked at 23.7 dB MER](docs/images/waterfall.png)

*A synthetic 8 MS/s test scene (`decdvb scene`): two DVB-S2 carriers — one
CCM, one ACM switching QPSK 1/2 → 8PSK 3/5 → 16APSK 2/3 — a plain QPSK carrier
and a CW tone. Every carrier was found and identified blind.*

> Early development. [`docs/DESIGN.md`](docs/DESIGN.md) holds the scope and the
> milestone plan, [`docs/STATUS.md`](docs/STATUS.md) what is done.

## What works now

| | |
|---|---|
| Waterfall + spectrum over the whole span, zoom and pan | ✅ |
| Carrier detection: every carrier marked with its symbol rate | ✅ |
| VFOs: draw, drag, resize, click a carrier to claim it; one thread each | ✅ |
| **Identify** — blind symbol rate, roll-off, constellation, DVB-S2 detection | ✅ |
| DVB-S2 PL demodulation: frame lock, MODCOD per frame (ACM) | ✅ |
| Carrier recovery: a locked constellation and MER, for Identify and DVB-S2 VFOs | ✅ |
| **Generic PSK/APSK → symbols** (`.bin`, one byte per symbol), for non-DVB carriers | ✅ |
| Narrow carriers: VFOs down to 500 Hz, ~10 kBd SCPC carriers lock | ✅ |
| IQ recorder and spectrum-only VFOs | ✅ |
| IQ file replay (`cs8`, `cs16`, `cf32`), rate/centre from file names | ✅ |
| **Live HackRF One**, 2–20 MS/s, pure Rust over USB (no DLLs), LNB LO | ✅ |
| LDPC + BCH → BBFRAMEs | M2 |
| All S2X MODCODs, VL-SNR | M3 |
| GSE → IP → PCAP, MPEG-TS | M4 |
| Modulator (HackRF TX / IQ file) | M6 |

## Using it

```bash
cargo run --release -p decdvb-gui -- capture_8Msps.cs8
```

Or open a capture from the toolbar, or drop it on the window. The sample rate,
centre frequency and format are read from the file name when it carries them
(gqrx, SDR++ and DecDVB's own recordings do); otherwise set them in the toolbar.

On the waterfall:

| | |
|---|---|
| **wheel** | zoom about the cursor |
| **right- or middle-drag** | pan |
| **drag on empty space** | draw a new VFO |
| **double-click** | drop a VFO |
| **click a green bracket** | claim a detected carrier with a VFO sized to fit |
| **drag a VFO** / **its edge** | move it / resize it |
| **click** (VFO selected) | tune it there |
| **✕** on a VFO's label, or **Delete** | remove it |

The big readout at the top is the centre frequency, as in SDR++: wheel over a
digit or click its upper/lower half to step it, right-click to round. With the
HackRF running it retunes the radio.

The side bar lists the VFOs with their CPU load (✕ removes one), and shows the
selected one's settings, its Identify result or demodulator state (lock, MER,
residual offset), a carrier-locked constellation and a zoomed spectrum.

### Generic PSK → symbols

For carriers that are not DVB-S2 — SCPC data, telemetry, DVB-S — a VFO can
lock the carrier and write its hard-decided symbols to
`decdvb-<VFO>-<freq>Hz-<rate>Bd-<modulation>-<time>.bin`, one byte per symbol:
the symbol's bit label under the DVB-S2 mapping (BPSK: 0 = +1). The symbol rate
and constellation come from Identify, or set them by hand. Without a preamble
the carrier phase is ambiguous by the constellation's symmetry (90° for QPSK),
so the labels may be a fixed rotation of the sent ones — a sync word found
offline resolves it.

### What Identify reports

It splits what it *knows* from what it *guesses*. **DVB-S2** is reported as a
fact — PLHEADERs found repeating exactly where their own PLS codes predict the
next frame — with the MODCODs in use, pilots, frame lengths, and CCM vs ACM.
Anything else gets measurements (symbol rate, roll-off, an estimated
constellation) and a labelled guess: plain QPSK reads *"possibly DVB-S (not
verified)"*, because confirming DVB-S needs a Viterbi decoder DecDVB does not
have. A carrier too slow to show three frames in the first look is marked
*provisional* while it listens longer.

### Command line

```bash
decdvb scan capture_8Msps.cs8     # find every carrier and identify each
decdvb scene                      # write the 8 MS/s test scene above
decdvb modcods                    # the DVB-S2 MODCOD table
```

## Build

Rust 1.95 or newer:

```bash
cargo build --release
```

### HackRF One

Click **📡 HackRF** in the toolbar, set the frequency, sample rate and gains,
and **Start**. Frequency and gains apply live. **LNB LO** only labels the axis
(RF = tuned + LO): 9750 MHz for a QO-100 or Ku low-band LNB, 0 without one.

The driver is pure Rust over USB ([`seify-hackrfone`](https://crates.io/crates/seify-hackrfone)
on `nusb`): no libhackrf, no libusb, nothing to install beyond the WinUSB driver
the HackRF already uses on Windows (Zadig, or the official tools). DecDVB only
ever **receives**, and keeps the antenna-port power **off** — feed an LNB from
an external inserter.

## Scope

Everything one HackRF One can carry: symbol rates to roughly 15 MS/s, normal /
short / medium FECFRAMEs, all DVB-S2 and S2X MODCODs up to 256APSK, VL-SNR with
pi/2-BPSK, roll-offs 0.35 down to 0.05, superframing (Annex E), multistream
(ISI) and the gold-code index. Channel bonding (Annex D) and wideband
time-slicing (Annex M) are out of scope — they need more spectrum than one
HackRF has. Wide broadcast transponders (27–45 MS/s) exceed the HackRF's USB
throughput, so those are handled from recorded IQ only.

## Credits

Written from ETSI **EN 302 307-1** (DVB-S2) and **EN 302 307-2** (DVB-S2X), with
algorithms studied from and credited to:

- [gr-dvbs2rx](https://github.com/igorauad/gr-dvbs2rx) (GPL-3) — PL framing,
  frame sync, LDPC/BCH
- [leansdr / leandvb](https://github.com/pabr/leansdr) (GPL-3) — constellations
- [gr-dtv](https://github.com/gnuradio/gnuradio) (GPL-3) — modulator reference
- [dontlookup](https://github.com/ucsdsysnet/dontlookup) (MIT) — GSE/IP
  de-encapsulation, including the proprietary header-length and split
  fragment-ID variants found on real carriers
- DecDRM — the waterfall's GPU ring texture

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).
