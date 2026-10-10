//! `decdvb decode`: run one decoder over a capture and report what it finds
//! — the same VFO the GUI runs, fed from the file, with a status line every
//! couple of seconds and a full report at the end (structure found, payload,
//! E1 timeslots, text, frame structure).
//!
//! With `--hackrf <MHz>` the source is the HackRF instead (receive only, its
//! antenna power forced off): a status line every 30 s, a report at the end
//! (`--seconds`) — for watching a carrier unattended, e.g. a FastLink link
//! with `--record-on-activity` until a voice channel speaks.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use decdvb_core::{Modulation, SampleFormat};
use decdvb_engine::{DecoderKind, Engine, EngineOptions, SourceState, VfoSettings, VfoStatus};
use decdvb_io::{IqFileReader, IqSource};

pub struct DecodeArgs {
    pub file: Option<PathBuf>,
    /// Live: the HackRF's centre frequency, MHz, and its gains.
    pub hackrf: Option<f64>,
    pub lna: u16,
    pub vga: u16,
    pub amp: bool,
    /// Stop after this long.
    pub seconds: Option<f64>,
    /// fastlink: record while a TDM channel is active.
    pub record_on_activity: bool,
    pub format: Option<SampleFormat>,
    pub rate: Option<f64>,
    pub decoder: String,
    pub offset: f64,
    pub bandwidth: Option<f64>,
    pub symbol_rate: Option<f64>,
    pub modulation: Option<String>,
    pub out: Option<PathBuf>,
    pub fast: bool,
    pub e1_record: Option<u8>,
    /// psk: how the .bin numbers the symbols.
    pub labels: String,
    /// cid: low-SNR mode.
    pub low_snr: bool,
    /// cid: the clock correction, ppm.
    pub clock_ppm: f64,
    /// psk: bits rather than symbol bytes.
    pub bits: bool,
}

fn decoder_by_name(name: &str) -> Result<DecoderKind> {
    Ok(match name {
        "id" | "identify" => DecoderKind::Identify,
        "ip" | "gse" => DecoderKind::Dvbs2Ip,
        "ts" => DecoderKind::Dvbs2Ts,
        "dvbs" => DecoderKind::DvbsTs,
        "tpc" | "tpc2964" => DecoderKind::Tpc2964,
        "fastlink" | "fl" => DecoderKind::FastLink,
        "viterbi" | "vit" => DecoderKind::Viterbi,
        "cid" | "carrier-id" => DecoderKind::CarrierId,
        "psk" => DecoderKind::PskSymbols,
        other => bail!("unknown decoder `{other}` (id, ip, ts, dvbs, tpc, psk)"),
    })
}

fn modulation_by_name(name: &str) -> Result<Modulation> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "bpsk" => Modulation::Bpsk,
        "qpsk" => Modulation::Qpsk,
        "8psk" => Modulation::Psk8,
        "16apsk" => Modulation::Apsk16,
        "32apsk" => Modulation::Apsk32,
        "8qam" => Modulation::Qam8,
        "16qam" => Modulation::Qam16,
        "64qam" => Modulation::Qam64,
        other => bail!("unknown modulation `{other}`"),
    })
}

/// The HackRF at `mhz`, `rate` samples a second.
#[cfg(feature = "hackrf")]
fn hackrf_source(a: &DecodeArgs, mhz: f64, rate: f64) -> Result<Box<dyn IqSource>> {
    use decdvb_io::{HackRfGains, HackRfSettings, HackRfSource};
    let src = HackRfSource::open(HackRfSettings {
        center_hz: (mhz * 1e6).round() as u64,
        sample_rate: rate.round() as u32,
        gains: HackRfGains {
            amp: a.amp,
            lna_db: a.lna,
            vga_db: a.vga,
        }
        .snapped(),
    })?;
    Ok(Box::new(src))
}

#[cfg(not(feature = "hackrf"))]
fn hackrf_source(_: &DecodeArgs, _: f64, _: f64) -> Result<Box<dyn IqSource>> {
    bail!("this build has no HackRF support (the `hackrf` feature)")
}

pub fn decode(a: DecodeArgs) -> Result<()> {
    let kind = decoder_by_name(&a.decoder)?;
    let live = a.hackrf.is_some();
    let (source, rate): (Box<dyn IqSource>, f64) = match (a.hackrf, &a.file) {
        (Some(mhz), _) => {
            let rate = a.rate.unwrap_or(2e6);
            let src = hackrf_source(&a, mhz, rate)?;
            println!(
                "live from the HackRF at {mhz:.4} MHz, {:.3} MS/s, LNA {} dB, VGA {} dB{} \
                 (receive only)",
                rate / 1e6,
                a.lna,
                a.vga,
                if a.amp { ", amp on" } else { "" }
            );
            (src, rate)
        }
        (None, Some(file)) => {
            let (fmt, rate) = crate::capture_format_rate(file, a.format, a.rate)?;
            let reader = IqFileReader::open(file, fmt, rate, 1 << 16)
                .with_context(|| format!("opening {}", file.display()))?;
            println!("decoding {} at {:.3} MS/s", reader.describe(), rate / 1e6);
            (Box::new(reader), rate)
        }
        (None, None) => bail!("give a capture, or --hackrf <MHz>"),
    };

    // Real time by default: a VFO that falls behind drops blocks, and a
    // decoder fed with holes reports them as faults of the signal.
    let mut eng = Engine::start(
        source,
        EngineOptions {
            realtime: !a.fast || live,
            loop_file: false,
            ..Default::default()
        },
    );
    let mut s = VfoSettings::new("decode", a.offset, a.bandwidth.unwrap_or(0.9 * rate), kind);
    s.symbol_rate = a.symbol_rate;
    if let Some(m) = &a.modulation {
        s.psk_modulation = Some(modulation_by_name(m)?);
    }
    {
        use decdvb_engine::psk::SymbolLabels;
        s.symbol_labels = match a.labels.to_ascii_lowercase().as_str() {
            "standard" | "std" => SymbolLabels::Standard,
            "natural" | "position" => SymbolLabels::Natural,
            "gray" | "grey" => SymbolLabels::Gray,
            other => bail!("--labels: `{other}` (want standard, natural or gray)"),
        };
    }
    s.cid_low_snr = a.low_snr;
    s.clock_ppm = a.clock_ppm;
    s.psk_bits = a.bits;
    if let Some(dir) = &a.out {
        // Record throughout, or (--record-on-activity) only while a channel
        // of a FastLink's multiplex is active.
        s.record = !a.record_on_activity;
        s.record_on_activity = a.record_on_activity;
        s.record_dir = dir.clone();
    } else if a.record_on_activity {
        bail!("--record-on-activity needs --out");
    }
    if a.e1_record.is_some() && a.out.is_none() {
        bail!("--e1-record needs --out");
    }
    s.e1_record = a.e1_record;
    let id = eng.add_vfo(s);

    let t0 = Instant::now();
    let mut last_line = Instant::now();
    let mut ended_at: Option<Instant> = None;
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let front = eng.front();
        if let SourceState::Failed(e) = &front.state {
            bail!("source failed: {e}");
        }
        if matches!(front.state, SourceState::Ended) && ended_at.is_none() {
            ended_at = Some(Instant::now());
        }
        let st = eng.vfo_status(id);
        let every = Duration::from_secs(if live { 30 } else { 2 });
        if last_line.elapsed() >= every {
            if let Some(st) = &st {
                let rec = st
                    .fec
                    .as_ref()
                    .filter(|_| a.record_on_activity)
                    .map(|f| format!(" · {} recordings", f.raw_triggered))
                    .unwrap_or_default();
                println!("[{:5.1} s] {}{rec}", t0.elapsed().as_secs_f64(), st.message);
            }
            last_line = Instant::now();
        }
        if a.seconds.is_some_and(|s| t0.elapsed().as_secs_f64() >= s) && ended_at.is_none() {
            ended_at = Some(Instant::now());
        }
        // After the end, give the FEC thread a moment to drain.
        if ended_at.is_some_and(|t| t.elapsed() >= Duration::from_secs(3)) {
            match st {
                Some(st) => report(&st),
                None => println!("no status from the VFO"),
            }
            return Ok(());
        }
    }
}

fn report(st: &VfoStatus) {
    println!("\n== {}", st.message);
    if let Some(c) = &st.carrier {
        println!(
            "carrier: {} {}, MER {:.1} dB, offset {:+.0} Hz",
            c.modulation.name(),
            if c.locked { "locked" } else { "not locked" },
            c.mer_db,
            c.offset_hz
        );
    }
    if let Some(rs) = st.symbol_rate {
        println!("symbol rate: {:.1} S/s", rs);
    }
    if let Some(id) = &st.identification {
        println!("identify: {}", id.summary());
        if let Some(p) = &id.frame_structure {
            println!(
                "frames: {} (over {} frames, {:.0}× the noise)",
                p.describe(),
                p.frames,
                p.prominence
            );
        }
    }
    if let Some(t) = &st.text {
        print_bit_text("text", t);
    }
    if let Some(c) = &st.cid {
        use decdvb_engine::cid::{guid_mac, guid_text};
        let s = &c.stats;
        println!(
            "dvb-cid: {:.0} kchip/s, code {}, {} searches, {:+.1} Hz, {:.1} dB a bit, {} bits, {} frames ({} failed){}",
            c.chip_rate / 1e3,
            if s.acquired { "found" } else { "not found" },
            s.searches,
            s.offset_hz,
            s.snr_db,
            s.bits,
            s.frames,
            s.bad_frames,
            if c.wide_enough {
                ""
            } else {
                " — VFO narrower than the CID's band"
            }
        );
        let r = &s.report;
        if let Some(g) = r.guid {
            println!("  identifier {}", guid_text(g));
            if let Some(m) = guid_mac(g) {
                println!("  MAC {m}");
            }
        }
        if let (Some(lat), Some(lon)) = (r.latitude(), r.longitude()) {
            println!("  position {lat:.5}, {lon:.5}");
        }
        if let Some(t) = r.telephone() {
            println!("  telephone {t}");
        }
        if let Some(t) = r.user_text() {
            println!("  text \"{t}\"");
        }
    }
    let Some(f) = &st.fec else { return };
    if let Some(t) = &f.tpc {
        println!(
            "tpc: UW {}, {}, structure {}, fit {:.0} %, {} frames ({} decoded, {} failed), BER {:.2e}, {} slips, {} UW misses",
            if t.uw_locked { "found" } else { "not found" },
            t.orientation.as_deref().unwrap_or("-"),
            t.structure.as_deref().unwrap_or("not identified"),
            t.fit * 100.0,
            t.frames,
            t.decoded,
            t.failed,
            t.channel_ber(),
            t.slips,
            t.uw_misses
        );
    }
    if let Some(v) = &f.viterbi {
        println!(
            "viterbi: rate {}, {}, channel BER {:.2e}, {} bits, {} searches, {} relocks, {} turns followed",
            v.rate.map_or("not found", |r| r.name()),
            v.orientation.as_deref().unwrap_or("-"),
            v.channel_ber,
            v.bits,
            v.searches,
            v.losses,
            v.turns
        );
    }
    if let Some(t) = &f.fastlink {
        println!(
            "fastlink: sync {}, {}, {} frames, {} codewords ({} decoded, {} failed), BER {:.2e}, {} slips, {} sync misses",
            if t.locked { "found" } else { "not found" },
            t.orientation.as_deref().unwrap_or("-"),
            t.frames,
            t.codewords,
            t.decoded,
            t.failed,
            t.channel_ber(),
            t.slips,
            t.uw_misses
        );
    }
    if let Some(p) = &f.payload {
        print_payload(p);
    }
    if let Some(t) = &f.tdm {
        print_tdm(t);
    }
    if let Some(e) = &f.e1 {
        println!(
            "e1: {} · {} frames · {} FAS errors · {} losses{}",
            if e.stats.locked {
                "aligned"
            } else {
                "not aligned"
            },
            e.stats.frames,
            e.stats.fas_errors,
            e.stats.losses,
            if e.stats.cas { " · CAS" } else { "" }
        );
        println!("  {}", e.source);
        if let Some((p, n)) = &e.record_file {
            println!("  recorded to {} ({n} bytes)", p.display());
        }
        for (ts, db) in e.levels_db.iter().enumerate().skip(1) {
            if *db > -60.0 {
                use decdvb_modem::e1::Coding;
                let note = match e.coding.get(ts).copied().unwrap_or_default() {
                    Coding::SubRate(m) => format!(
                        " · not G.711: only bits {} change (sub-rate channels or compressed voice)",
                        Coding::bits_text(m)
                    ),
                    Coding::Steady => " · steady (idle pattern or tone)".into(),
                    _ => String::new(),
                };
                println!("  TS {ts:2}: {db:6.1} dBFS{note}");
            }
        }
    }
    if let Some(t) = &f.text {
        print_bit_text("data text", t);
    }
    if let Some(g) = &f.gse {
        println!(
            "ip: {} packets, {} flows, {:.1} kbit/s",
            g.packets,
            g.flows,
            g.ip_bps / 1e3
        );
        if let Some(l) = &g.link {
            println!("  via {l}");
        }
        for fl in g.top.iter().take(8) {
            println!("  {fl:?}");
        }
        for (group, port, sdp) in &g.stations {
            println!(
                "  station \"{}\" at {group}:{port} ({})",
                sdp.name.as_deref().unwrap_or("no name"),
                sdp.codec().label()
            );
        }
        for a in &g.audio {
            println!("  audio: {} ({:?})", a.name(), a.codec);
        }
        print_byte_text("ip text", &g.text);
    }
    if let Some(t) = &f.ts {
        println!("ts: {} packets, {} CC errors", t.packets, t.cc_errors);
        for p in &t.report.programmes {
            println!("  programme {:?}", p.name);
        }
        print_byte_text("ts text", &t.text);
    }
}

fn print_bit_text(what: &str, t: &decdvb_engine::TextView) {
    match &t.best {
        Some(how) => {
            println!("{what}: read as {how}");
            for s in t.strings.iter().rev().take(12) {
                println!("  {s}");
            }
        }
        None => println!(
            "{what}: nothing stands out ({} bytes, {} readings)",
            t.bytes, t.readings
        ),
    }
    for (how, s) in t.candidates.iter().take(5) {
        println!("  candidate {s:?}  [{how}]");
    }
}

fn print_byte_text(what: &str, t: &decdvb_engine::ByteTextView) {
    if t.repeated.is_empty() && t.recent.is_empty() {
        return;
    }
    println!("{what}:");
    for (s, n) in t.repeated.iter().take(10) {
        println!("  ×{n:<4} {s}");
    }
    for s in t.recent.iter().take(10) {
        println!("  … {s}");
    }
}

/// The payload stage's report.
pub(crate) fn print_payload(p: &decdvb_modem::payload::PayloadStats) {
    println!(
        "payload: {} ({} frames looked at; HDLC {} good / {} bad; TS {})",
        p.found.as_deref().unwrap_or("not recognised"),
        p.probed,
        p.hdlc_good,
        p.hdlc_bad,
        p.ts_packets
    );
    if let Some(e) = &p.paradise {
        println!(
            "  paradise framing: {}, {} groups, {} FAW errors, {} losses, ESC busy {:.1} %",
            if e.locked { "aligned" } else { "searching" },
            e.groups,
            e.faw_errors,
            e.losses,
            100.0 * e.esc_busy as f64 / e.groups.max(1) as f64
        );
    }
    if let Some(e) = &p.ibs {
        let hex = |v: &[u8; 4]| v.map(|b| format!("{b:02x}")).join(" ");
        println!(
            "  ibs framing: {}, {} frames, {} misaligned, {} losses, overhead cycle {}, service bits {}",
            if e.locked { "aligned" } else { "searching" },
            e.frames,
            e.misaligned,
            e.losses,
            hex(&e.cycle),
            hex(&e.varying)
        );
    }
}

/// The 257-bit TDM multiplex's report.
pub(crate) fn print_tdm(t: &decdvb_modem::tdm257::TdmStats) {
    println!(
        "tdm 257: {} · {} frames (2 ms) · {} alignment-bit errors · {} losses · side data {}",
        if t.locked { "aligned" } else { "searching" },
        t.frames,
        t.faw_errors,
        t.losses,
        t.side_data
            .iter()
            .map(|b| char::from(b'0' + b))
            .collect::<String>()
    );
    for c in &t.calls {
        println!(
            "  call at {:.1} s for {:.1} s{} on channels {}",
            c.start as f64 * 0.002,
            c.seconds(),
            if c.open { " (still going)" } else { "" },
            c.channel_list()
        );
    }
    for (c, ch) in t.channels.iter().enumerate() {
        println!(
            "  channel {c:2} (bit {c:2} of each word): {:12} changes 20 ms {:5.1} % · 4 ms {:5.1} % · ones {:4.1} %",
            ch.state.label(),
            100.0 * ch.change_20ms,
            100.0 * ch.change_4ms,
            100.0 * ch.ones
        );
    }
}
