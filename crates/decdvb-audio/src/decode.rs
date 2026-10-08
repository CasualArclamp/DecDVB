//! Decoding [`Unit`]s to stereo PCM.
//!
//! MPEG audio layers I–III and AAC-LC are decoded by Symphonia (pure Rust,
//! MPL-2.0); PCM arrives already decoded. Everything comes out as
//! interleaved stereo f32 — mono is copied to both sides, and of more than
//! two channels the first two are kept — so the rest of the player has one
//! shape to deal with.

use symphonia_bundle_mp3::MpaDecoder;
use symphonia_codec_aac::AacDecoder;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_AAC, CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3,
};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

use crate::aac::{AacConfig, fmt_khz};
use crate::depay::Unit;
use crate::es::MpaHeader;

/// Stereo audio at one sample rate.
#[derive(Debug, Default, Clone)]
pub struct Block {
    pub rate: u32,
    /// Interleaved L, R.
    pub samples: Vec<f32>,
}

/// Decodes whatever a stream's units are.
#[derive(Default)]
pub struct Decoder {
    mpa: Option<(MpaHeader, MpaDecoder)>,
    aac: Option<(AacConfig, AacDecoder)>,
    scratch: Vec<f32>,
    /// Units decoded, and those that failed.
    pub decoded: u64,
    pub errors: u64,
    /// What is being decoded ("MPEG-1 Layer II · 192 kbit/s · 48 kHz ·
    /// stereo"), once something has been.
    pub description: Option<String>,
    /// Why the last unit could not be decoded.
    pub last_error: Option<String>,
}

fn options() -> AudioDecoderOptions {
    let mut o = AudioDecoderOptions::default();
    // Nothing here has encoder delay or padding to trim.
    o.gapless = false;
    o
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode one unit into `out` (replacing what it held). False if
    /// nothing came out.
    pub fn decode(&mut self, unit: &Unit, out: &mut Block) -> bool {
        out.samples.clear();
        let r = match unit {
            Unit::Mpa(f) => self.mpa(f, out),
            Unit::Aac(cfg, au) => self.aac(cfg, au, out),
            Unit::Pcm {
                rate,
                channels,
                samples,
            } => {
                out.rate = *rate;
                to_stereo(samples, *channels as usize, &mut out.samples);
                self.description = Some(format!(
                    "PCM · {} kHz · {}",
                    fmt_khz(*rate),
                    if *channels == 1 { "mono" } else { "stereo" }
                ));
                Ok(())
            }
            Unit::Ts(_) => Err("MPEG-TS is recorded, not played".into()),
        };
        match r {
            Ok(()) if !out.samples.is_empty() => {
                self.decoded += 1;
                true
            }
            Ok(()) => false,
            Err(e) => {
                self.errors += 1;
                self.last_error = Some(e);
                false
            }
        }
    }

    fn mpa(&mut self, f: &[u8], out: &mut Block) -> Result<(), String> {
        let h = MpaHeader::parse(f).ok_or("not an MPEG audio frame")?;
        let same = self.mpa.as_ref().is_some_and(|(o, _)| {
            (o.layer, o.sample_rate, o.channels) == (h.layer, h.sample_rate, h.channels)
        });
        if !same {
            let mut p = AudioCodecParameters::new();
            p.for_codec(match h.layer {
                1 => CODEC_ID_MP1,
                2 => CODEC_ID_MP2,
                _ => CODEC_ID_MP3,
            });
            let d = MpaDecoder::try_new(&p, &options()).map_err(|e| e.to_string())?;
            self.mpa = Some((h, d));
        }
        let (_, d) = self.mpa.as_mut().unwrap();
        let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), f);
        let buf = d.decode_ref(&pkt).map_err(|e| e.to_string())?;
        let ch = buf.spec().channels().count();
        out.rate = buf.spec().rate();
        buf.copy_to_vec_interleaved(&mut self.scratch);
        to_stereo(&self.scratch, ch, &mut out.samples);
        self.description = Some(format!(
            "MPEG-{} Layer {} · {} kbit/s · {} kHz · {}",
            if h.version == 25 {
                "2.5".to_string()
            } else {
                h.version.to_string()
            },
            ["I", "II", "III"][h.layer as usize - 1],
            h.bitrate_kbps,
            fmt_khz(h.sample_rate),
            if h.channels == 1 { "mono" } else { "stereo" }
        ));
        Ok(())
    }

    fn aac(&mut self, cfg: &AacConfig, au: &[u8], out: &mut Block) -> Result<(), String> {
        if !cfg.playable() {
            return Err(format!("{} cannot be decoded here", cfg.describe()));
        }
        if self
            .aac
            .as_ref()
            .is_none_or(|(c, _)| c.core_asc() != cfg.core_asc())
        {
            let mut p = AudioCodecParameters::new();
            p.for_codec(CODEC_ID_AAC)
                .with_extra_data(cfg.core_asc().into_boxed_slice());
            let d = AacDecoder::try_new(&p, &options()).map_err(|e| e.to_string())?;
            self.aac = Some((*cfg, d));
        }
        let (_, d) = self.aac.as_mut().unwrap();
        let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), au);
        let buf = d.decode_ref(&pkt).map_err(|e| e.to_string())?;
        let ch = buf.spec().channels().count();
        out.rate = buf.spec().rate();
        buf.copy_to_vec_interleaved(&mut self.scratch);
        to_stereo(&self.scratch, ch, &mut out.samples);
        let mut d = cfg.describe();
        if cfg.sbr_rate.is_some() {
            d.push_str(" (playing the AAC-LC core)");
        }
        self.description = Some(d);
        Ok(())
    }
}

/// Interleaved `channels`-channel audio to interleaved stereo.
fn to_stereo(x: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        0 => {}
        1 => out.extend(x.iter().flat_map(|&v| [v, v])),
        2 => out.extend_from_slice(x),
        n => out.extend(x.chunks_exact(n).flat_map(|c| [c[0], c[1]])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aac::silent_aac_au;
    use decdvb_ip::mcast::silent_mp2_frame;

    #[test]
    fn decodes_silent_mp2() {
        let mut d = Decoder::new();
        let mut b = Block::default();
        assert!(
            d.decode(&Unit::Mpa(silent_mp2_frame()), &mut b),
            "{:?}",
            d.last_error
        );
        assert_eq!(b.rate, 48_000);
        assert_eq!(b.samples.len(), 2 * 1152);
        assert!(b.samples.iter().all(|v| v.abs() < 1e-6));
        assert!(
            d.description
                .as_deref()
                .unwrap()
                .starts_with("MPEG-1 Layer II")
        );
    }

    #[test]
    fn decodes_silent_aac_mono_and_stereo() {
        for (asc, ch) in [([0x11u8, 0x88], 1u8), ([0x11, 0x90], 2)] {
            let cfg = AacConfig::from_bytes(&asc).unwrap();
            assert_eq!(cfg.channel_config, ch);
            let mut d = Decoder::new();
            let mut b = Block::default();
            assert!(
                d.decode(&Unit::Aac(cfg, silent_aac_au(ch)), &mut b),
                "{ch} ch: {:?}",
                d.last_error
            );
            assert_eq!(b.rate, 48_000);
            assert_eq!(b.samples.len(), 2 * 1024);
            assert!(b.samples.iter().all(|v| v.abs() < 1e-6));
        }
    }

    #[test]
    fn he_aac_plays_its_core() {
        // HE-AAC, 24 kHz core, stereo, 48 kHz with SBR.
        let cfg = AacConfig::from_bytes(&[0x2B, 0x11, 0x88, 0x00]).unwrap();
        let mut d = Decoder::new();
        let mut b = Block::default();
        assert!(
            d.decode(&Unit::Aac(cfg, silent_aac_au(2)), &mut b),
            "{:?}",
            d.last_error
        );
        assert_eq!(b.rate, 24_000);
        assert!(d.description.as_deref().unwrap().contains("core"));
    }

    #[test]
    fn pcm_mono_becomes_stereo() {
        let mut d = Decoder::new();
        let mut b = Block::default();
        let u = Unit::Pcm {
            rate: 8000,
            channels: 1,
            samples: vec![0.5, -0.25],
        };
        assert!(d.decode(&u, &mut b));
        assert_eq!(b.samples, vec![0.5, 0.5, -0.25, -0.25]);
    }
}
