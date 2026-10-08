//! IQ file replay and recording.
//!
//! Formats are the ones the SDR world actually hands around: `cs8` (the
//! HackRF's native signed 8-bit interleaved), `cs16` and `cf32`.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use decdvb_core::{Iq, Result, SampleFormat, bytes_to_iq};

use crate::IqSource;

/// Guess the sample format from a file extension, following the common
/// conventions (`.cs8`/`.s8`/`.iq8`, `.cs16`/`.s16`, `.cf32`/`.fc32`).
///
/// Returns `None` when the extension says nothing; the caller should then ask
/// the user rather than guess, since reading cs16 as cs8 silently "works".
pub fn format_from_path(path: &Path) -> Option<SampleFormat> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "cs8" | "s8" | "iq8" | "i8" => Some(SampleFormat::Cs8),
        "cs16" | "s16" | "iq16" => Some(SampleFormat::Cs16),
        "cf32" | "fc32" | "f32" | "iq32" => Some(SampleFormat::Cf32),
        _ => None,
    }
}

/// Reads interleaved IQ from a file in fixed-size blocks.
pub struct IqFileReader {
    inner: BufReader<File>,
    fmt: SampleFormat,
    sample_rate: f64,
    center_freq: f64,
    /// Scratch byte buffer, sized to `block_samples * bytes_per_sample`.
    raw: Vec<u8>,
    name: String,
}

impl IqFileReader {
    /// Open `path` as `fmt`, reading `block_samples` complex samples per
    /// [`IqSource::read`] call.
    pub fn open(
        path: &Path,
        fmt: SampleFormat,
        sample_rate: f64,
        block_samples: usize,
    ) -> Result<Self> {
        let file = File::open(path)?;
        Ok(IqFileReader {
            inner: BufReader::with_capacity(1 << 20, file),
            fmt,
            sample_rate,
            center_freq: 0.0,
            raw: vec![0u8; block_samples * fmt.bytes_per_sample()],
            name: path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
        })
    }

    /// Set the centre frequency reported to the GUI (files carry no metadata).
    pub fn with_center_freq(mut self, hz: f64) -> Self {
        self.center_freq = hz;
        self
    }
}

impl IqSource for IqFileReader {
    fn read(&mut self, out: &mut Vec<Iq>) -> Result<usize> {
        // Read as much of the block as the file has left. `read` may return
        // short reads even mid-file, so loop until the buffer is full or EOF.
        let mut filled = 0;
        while filled < self.raw.len() {
            match self.inner.read(&mut self.raw[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        // Drop any trailing partial sample.
        let usable = filled - (filled % self.fmt.bytes_per_sample());
        bytes_to_iq(&self.raw[..usable], self.fmt, out);
        Ok(out.len())
    }

    fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    fn center_freq(&self) -> f64 {
        self.center_freq
    }

    fn describe(&self) -> String {
        format!(
            "{} ({:?}, {:.3} MS/s)",
            self.name,
            self.fmt,
            self.sample_rate / 1e6
        )
    }

    fn rewind(&mut self) -> Result<bool> {
        self.inner.seek(SeekFrom::Start(0))?;
        Ok(true)
    }
}

/// Writes interleaved IQ to a file — recording a capture, or the modulator's
/// output when not transmitting live.
pub struct IqFileWriter {
    inner: BufWriter<File>,
    fmt: SampleFormat,
}

impl IqFileWriter {
    pub fn create(path: &Path, fmt: SampleFormat) -> Result<Self> {
        Ok(IqFileWriter {
            inner: BufWriter::with_capacity(1 << 20, File::create(path)?),
            fmt,
        })
    }

    /// Append samples, clamping to the target format's range.
    pub fn write(&mut self, samples: &[Iq]) -> Result<()> {
        match self.fmt {
            SampleFormat::Cs8 => {
                for s in samples {
                    let i = (s.re * 127.0).clamp(-128.0, 127.0) as i8;
                    let q = (s.im * 127.0).clamp(-128.0, 127.0) as i8;
                    self.inner.write_all(&[i as u8, q as u8])?;
                }
            }
            SampleFormat::Cs16 => {
                for s in samples {
                    let i = (s.re * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    let q = (s.im * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    self.inner.write_all(&i.to_le_bytes())?;
                    self.inner.write_all(&q.to_le_bytes())?;
                }
            }
            SampleFormat::Cf32 => {
                for s in samples {
                    self.inner.write_all(&s.re.to_le_bytes())?;
                    self.inner.write_all(&s.im.to_le_bytes())?;
                }
            }
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        self.inner.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdvb_core::Iq;

    #[test]
    fn extension_guessing() {
        assert_eq!(
            format_from_path(Path::new("cap.cs8")),
            Some(SampleFormat::Cs8)
        );
        assert_eq!(
            format_from_path(Path::new("cap.CF32")),
            Some(SampleFormat::Cf32)
        );
        assert_eq!(format_from_path(Path::new("cap.bin")), None);
    }

    #[test]
    fn write_then_read_round_trip() {
        let dir = std::env::temp_dir();
        let path = dir.join("decdvb-io-roundtrip.cf32");
        let want: Vec<Iq> = (0..64)
            .map(|k| Iq::new(k as f32 / 64.0, -(k as f32) / 128.0))
            .collect();

        let mut w = IqFileWriter::create(&path, SampleFormat::Cf32).unwrap();
        w.write(&want).unwrap();
        w.finish().unwrap();

        let mut r = IqFileReader::open(&path, SampleFormat::Cf32, 1e6, 64).unwrap();
        let mut got = Vec::new();
        let n = r.read(&mut got).unwrap();
        assert_eq!(n, want.len());
        assert_eq!(got, want);

        // Second read hits EOF.
        assert_eq!(r.read(&mut got).unwrap(), 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn short_final_block_is_truncated_to_whole_samples() {
        let dir = std::env::temp_dir();
        let path = dir.join("decdvb-io-partial.cs8");
        // 5 bytes = 2 whole cs8 samples + 1 stray byte.
        std::fs::write(&path, [1u8, 2, 3, 4, 5]).unwrap();
        let mut r = IqFileReader::open(&path, SampleFormat::Cs8, 1e6, 16).unwrap();
        let mut got = Vec::new();
        assert_eq!(r.read(&mut got).unwrap(), 2);
        let _ = std::fs::remove_file(&path);
    }
}
