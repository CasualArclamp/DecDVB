//! `decdvb decode`: run one decoder over a capture and report what it finds
//! — the same VFO the GUI runs, fed from the file, with a status line every
//! couple of seconds and a full report at the end (structure found, payload,
//! E1 timeslots, text, frame structure).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use decdvb_core::{Modulation, SampleFormat};
use decdvb_engine::{DecoderKind, Engine, EngineOptions, SourceState, VfoSettings, VfoStatus};
use decdvb_io::{IqFileReader, IqSource, format_from_path};

pub struct DecodeArgs {
    pub file: PathBuf,
    pub format: Option<SampleFormat>,
    pub rate: Option<f64>,
    pub decoder: String,
    pub offset: f64,
    pub bandwidth: Option<f64>,
    pub symbol_rate: Option<f64>,
    pub modulation: Option<String>,
    pub out: Option<PathBuf>,
    pub fast: bool,
}

fn decoder_by_name(name: &str) -> Result<DecoderKind> {
    Ok(match name {
        "id" | "identify" => DecoderKind::Identify,
        "ip" | "gse" => DecoderKind::Dvbs2Ip,
        "ts" => DecoderKind::Dvbs2Ts,
        "dvbs" => DecoderKind::DvbsTs,
        "tpc" | "tpc2964" => DecoderKind::Tpc2964,
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

pub fn decode(a: DecodeArgs) -> Result<()> {
    let fmt = a
        .format
        .or_else(|| format_from_path(&a.file))
        .context("cannot tell the sample format from the extension — pass --format")?;
    let rate = a
        .rate
        .or_else(|| crate::rate_from_name(&a.file))
        .context("cannot tell the sample rate from the file name — pass --rate")?;
    let kind = decoder_by_name(&a.decoder)?;
    let reader = IqFileReader::open(&a.file, fmt, rate, 1 << 16)
        .with_context(|| format!("opening {}", a.file.display()))?;
    println!("decoding {} at {:.3} MS/s", reader.describe(), rate / 1e6);

    // Real time by default: a VFO that falls behind drops blocks, and a
    // decoder fed with holes reports them as faults of the signal.
    let mut eng = Engine::start(
        Box::new(reader),
        EngineOptions {
            realtime: !a.fast,
            loop_file: false,
            ..Default::default()
        },
    );
    let mut s = VfoSettings::new("decode", a.offset, a.bandwidth.unwrap_or(0.9 * rate), kind);
    s.symbol_rate = a.symbol_rate;
    if let Some(m) = &a.modulation {
        s.psk_modulation = Some(modulation_by_name(m)?);
    }
    if let Some(dir) = &a.out {
        s.record = true;
        s.record_dir = dir.clone();
    }
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
        if last_line.elapsed() >= Duration::from_secs(2) {
            if let Some(st) = &st {
                println!("[{:5.1} s] {}", t0.elapsed().as_secs_f64(), st.message);
            }
            last_line = Instant::now();
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
    if let Some(p) = &f.payload {
        println!(
            "payload: {} ({} frames looked at; HDLC {} good / {} bad; TS {})",
            p.found.as_deref().unwrap_or("not recognised"),
            p.probed,
            p.hdlc_good,
            p.hdlc_bad,
            p.ts_packets
        );
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
        for (ts, db) in e.levels_db.iter().enumerate().skip(1) {
            if *db > -60.0 {
                println!("  TS {ts:2}: {db:6.1} dBFS");
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
