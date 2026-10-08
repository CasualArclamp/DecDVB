# DecDVB

A **DVB-S2 / DVB-S2X** receiver in Rust, for a **HackRF One** or recorded IQ:
adaptive coding and modulation (**ACM**), **GSE** de-encapsulation to **IP** written
as a PCAP, **MPEG-TS** extraction, and a matching **modulator** for loopback and
ACM test signals.

> Early development. See [`docs/DESIGN.md`](docs/DESIGN.md) for the scope and the
> milestone plan; the README tracks what actually works.

## What works now

| | |
|---|---|
| IQ file replay (`cs8`, `cs16`, `cf32`) | ✅ |
| Spectrum + constellation display (GUI) | ✅ |
| Coding tables (`decdvb modcods`) | ✅ |
| Live HackRF front end | M1 |
| PL sync, PLHEADER/MODCOD decode | M1 |
| LDPC + BCH, BBFRAME output | M2 |
| All S2/S2X MODCODs, VL-SNR | M3 |
| GSE → IP → PCAP, TS extraction | M4 |
| Superframes, multistream, ACM polish | M5 |
| Modulator (HackRF TX / IQ file) | M6 |

## Build

```bash
cargo build --release
```

Run the GUI:

```bash
cargo run --release -p decdvb-gui
```

Or the CLI:

```bash
cargo run --release -p decdvb-cli -- analyse capture.cs8 --rate 2e6
```

The live HackRF front end is behind a feature flag so that building needs no SDR
SDK installed:

```bash
cargo run --release -p decdvb-gui --features hackrf
```

## Scope

Everything one HackRF One can carry: symbol rates to roughly 15 MS/s, normal /
short / medium FECFRAMEs, all DVB-S2 and S2X MODCODs up to 256APSK, VL-SNR with
pi/2-BPSK, roll-offs 0.35 down to 0.05, superframing (Annex E), multistream
(ISI) and the gold-code index. Channel bonding (Annex D) and wideband
time-slicing (Annex M) are out of scope — they need more spectrum than one
HackRF has.

Wide broadcast transponders (27–45 MS/s) exceed the HackRF's USB throughput, so
those are handled from recorded IQ only.

## Credits

Written from ETSI **EN 302 307-1** (DVB-S2) and **EN 302 307-2** (DVB-S2X), with
algorithms studied from and credited to:

- [gr-dvbs2rx](https://github.com/igorauad/gr-dvbs2rx) (GPL-3) — receiver, LDPC/BCH
- [leansdr / leandvb](https://github.com/pabr/leansdr) (GPL-3) — acquisition and sync
- [gr-dtv](https://github.com/gnuradio/gnuradio) (GPL-3) — modulator reference
- [dontlookup](https://github.com/ucsdsysnet/dontlookup) (MIT) — GSE/IP
  de-encapsulation, including the proprietary header-length and split
  fragment-ID variants found on real carriers

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).
