//! Recording a stream to a file, as broadcast — no decoding, no re-encoding.
//!
//! The file type follows what the stream carries, and is chosen on the first
//! frame (so the extension is right however the stream was sniffed):
//! - MPEG audio frames as they are: `.mp2` (`.mp1`, `.mp3` by layer);
//! - AAC of any carriage (ADTS, LATM, RFC 3640) as ADTS: `.aac`, which every
//!   player opens and which keeps HE-AAC's SBR and PS data intact;
//! - PCM as 16-bit `.wav`;
//! - MPEG-TS as `.ts`.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use decdvb_ip::AudioStream;

use crate::depay::{Depacketizer, Unit};
use crate::es::MpaHeader;

/// Writes one stream to a file.
pub struct AudioRecorder {
    depay: Depacketizer,
    dir: PathBuf,
    stem: String,
    file: Option<BufWriter<File>>,
    path: Option<PathBuf>,
    /// WAV: (sample rate, channels, data bytes) for the header.
    wav: Option<(u32, u16, u64)>,
    units: Vec<Unit>,
    /// Bytes written.
    pub bytes: u64,
    pub error: Option<String>,
}

impl AudioRecorder {
    /// Record `s` into `dir` as `<stem>.<ext>`; the file opens with the
    /// first frame. Fails at once for a stream that cannot be taken apart.
    pub fn start(s: &AudioStream, dir: &Path, stem: &str) -> Result<AudioRecorder, String> {
        Ok(AudioRecorder {
            depay: Depacketizer::new(s)?,
            dir: dir.to_path_buf(),
            stem: stem.to_string(),
            file: None,
            path: None,
            wav: None,
            units: Vec::new(),
            bytes: 0,
            error: None,
        })
    }

    /// The file, once opened.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// One UDP payload of the stream.
    pub fn packet(&mut self, udp_payload: &[u8]) {
        if self.error.is_some() {
            return;
        }
        let mut units = std::mem::take(&mut self.units);
        self.depay.packet(udp_payload, &mut units);
        for u in units.drain(..) {
            if let Err(e) = self.write(&u) {
                self.error = Some(e);
                self.file = None;
                break;
            }
        }
        self.units = units;
    }

    fn open(&mut self, u: &Unit) -> Result<(), String> {
        let ext = match u {
            Unit::Mpa(f) => match MpaHeader::parse(f).map(|h| h.layer) {
                Some(1) => "mp1",
                Some(3) => "mp3",
                _ => "mp2",
            },
            Unit::Aac(..) => "aac",
            Unit::Pcm { .. } => "wav",
            Unit::Ts(_) => "ts",
        };
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let path = self.dir.join(format!("{}.{ext}", self.stem));
        let f = File::create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut w = BufWriter::new(f);
        if let Unit::Pcm { rate, channels, .. } = u {
            w.write_all(&wav_header(*rate, *channels, 0))
                .map_err(|e| e.to_string())?;
            self.wav = Some((*rate, *channels, 0));
        }
        self.file = Some(w);
        self.path = Some(path);
        Ok(())
    }

    fn write(&mut self, u: &Unit) -> Result<(), String> {
        if self.file.is_none() {
            self.open(u)?;
        }
        let w = self.file.as_mut().unwrap();
        let n = match u {
            Unit::Mpa(f) | Unit::Ts(f) => {
                w.write_all(f).map_err(|e| e.to_string())?;
                f.len()
            }
            Unit::Aac(cfg, au) => {
                w.write_all(&cfg.adts_header(au.len()))
                    .and_then(|_| w.write_all(au))
                    .map_err(|e| e.to_string())?;
                au.len() + 7
            }
            Unit::Pcm { samples, .. } => {
                let mut b = Vec::with_capacity(samples.len() * 2);
                for &s in samples {
                    let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                    b.extend_from_slice(&v.to_le_bytes());
                }
                w.write_all(&b).map_err(|e| e.to_string())?;
                if let Some(wav) = &mut self.wav {
                    wav.2 += b.len() as u64;
                }
                b.len()
            }
        };
        self.bytes += n as u64;
        Ok(())
    }

    /// Flush, and complete a WAV header.
    fn finish(&mut self) -> std::io::Result<()> {
        let Some(mut w) = self.file.take() else {
            return Ok(());
        };
        w.flush()?;
        if let Some((rate, ch, data)) = self.wav {
            let mut f = w.into_inner().map_err(|e| e.into_error())?;
            f.seek(SeekFrom::Start(0))?;
            f.write_all(&wav_header(rate, ch, data))?;
        }
        Ok(())
    }
}

impl Drop for AudioRecorder {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// A 44-byte PCM WAV header for `data` bytes of 16-bit audio.
fn wav_header(rate: u32, channels: u16, data: u64) -> [u8; 44] {
    let data = data.min(u32::MAX as u64 - 36) as u32;
    let block = channels * 2;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * block as u32).to_le_bytes());
    h[32..34].copy_from_slice(&block.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aac::{loas_frame, silent_aac_au};
    use decdvb_ip::Codec;
    use decdvb_ip::mcast::{rtp_packet, silent_mp2_frame};

    fn stream(rtp: bool, pt: Option<u8>, codec: Codec) -> AudioStream {
        AudioStream {
            group: "239.1.1.1".parse().unwrap(),
            port: 5004,
            src: None,
            packets: 100,
            rate_bps: 1e5,
            rtp,
            pt,
            codec,
            sdp: None,
        }
    }

    fn dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("decdvb-rec-{tag}-{}", std::process::id()))
    }

    #[test]
    fn mp2_is_written_as_is() {
        let d = dir("mp2");
        let mut r =
            AudioRecorder::start(&stream(true, Some(14), Codec::MpegAudio), &d, "x").unwrap();
        for k in 0..10u16 {
            let mut p = vec![0, 0, 0, 0];
            p.extend_from_slice(&silent_mp2_frame());
            r.packet(&rtp_packet(14, k, 0, 1, &p));
        }
        let path = r.path().unwrap().to_path_buf();
        drop(r);
        let b = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(path.extension().unwrap(), "mp2");
        assert!(b.len() >= 9 * 384 && b.len().is_multiple_of(384));
        assert_eq!(&b[..384], &silent_mp2_frame()[..]);
    }

    #[test]
    fn latm_becomes_adts() {
        let d = dir("aac");
        let mut r = AudioRecorder::start(&stream(false, None, Codec::AacLatm), &d, "y").unwrap();
        let au = silent_aac_au(2);
        for _ in 0..5 {
            r.packet(&loas_frame(3, 2, &au));
        }
        let path = r.path().unwrap().to_path_buf();
        drop(r);
        let b = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(path.extension().unwrap(), "aac");
        let h = crate::es::AdtsHeader::parse(&b).unwrap();
        assert_eq!(h.frame_len, 7 + au.len());
        assert_eq!(&b[7..7 + au.len()], &au[..]);
    }

    #[test]
    fn pcm_makes_a_complete_wav() {
        let d = dir("wav");
        let mut r = AudioRecorder::start(&stream(true, Some(0), Codec::Pcm), &d, "z").unwrap();
        for k in 0..3u16 {
            r.packet(&rtp_packet(0, k, 0, 1, &[0xFF; 160]));
        }
        let path = r.path().unwrap().to_path_buf();
        drop(r);
        let b = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 8000);
        assert_eq!(
            u32::from_le_bytes(b[40..44].try_into().unwrap()),
            3 * 160 * 2
        );
        assert_eq!(b.len(), 44 + 3 * 160 * 2);
    }
}
