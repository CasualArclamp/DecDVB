//! A 128.5 kbit/s TDM multiplex found inside a Q-Flex FastLink carrier's
//! Paradise framing (what feeds the modem's data port; its make is not yet
//! known): 257-bit frames every 2 ms — one alignment bit, then 256 payload
//! bits read as sixteen 16-bit words at 8 kHz, each bit position of the
//! word an 8 kbit/s channel.
//!
//! Alignment bits, one a frame: on alternate frames the 7-bit Barker
//! sequence reversed (0100111) over and over, so a 14-frame (28 ms)
//! cycle; between them 7 data bits a cycle, repeating every 70 frames
//! (140 ms). All of this was found blind on one capture (STATUS.md, "The
//! Q-Flex's 128.5 kbit/s"); no specification is known.
//!
//! Each channel runs in 4 ms subframes of four octets, `D(n) D(n−1) S S`
//! (measured, STATUS.md "Q-Flex channels: 4 ms subframes"): one new data
//! octet, the previous one again, and a status octet twice — so 2 kbit/s
//! of new data a channel. Idle codec channels cycle five data octets
//! (a 40-bit frame every 20 ms); on the signalling channels S is 00 or FF.
//! A channel's subframes slip against the TDM frame now and then: each
//! source has its own clock.
//!
//! The receiver finds the frame by the alignment word, then meters every
//! channel: idle codec channels repeat a 160-bit (20 ms) frame, pattern
//! channels a 32-bit (4 ms) one, and speech should show as a channel that
//! stops repeating. Speech changes only the data half of each subframe —
//! about a quarter of the bits from one 20 ms to the next — so a channel
//! once seen idle as a codec counts as active as soon as it departs from
//! its idle frame at all.

/// Frame and payload bits, words a frame, channels.
pub const FRAME: usize = 257;
pub const PAYLOAD: usize = 256;
pub const WORDS: usize = 16;
pub const CHANNELS: usize = 16;
/// The alignment word, one bit every other frame.
pub const FAW: [u8; 7] = [0, 1, 0, 0, 1, 1, 1];
/// Frames searched for the alignment word (8 words' worth).
const SEARCH_FRAMES: usize = 2 * FAW.len() * 8;
/// Alignment bits wrong of the last 14 seen before searching again.
const LOSE: u32 = 5;
/// Bits a channel's meter looks back: a codec frame (20 ms) and a pattern
/// (4 ms); and its window (0.5 s at 8 kbit/s).
const CODEC_LAG: usize = 160;
const PATTERN_LAG: usize = 32;
const WINDOW: usize = 4000;
/// Changed at 20 ms: under this an idle codec, over the other active —
/// for any channel, and (much lower) for one seen idle as a codec.
const IDLE_CHANGE: f32 = 0.01;
const ACTIVE_CHANGE: f32 = 0.25;
const CODEC_ACTIVE_CHANGE: f32 = 0.05;
/// Bits a channel must have spent idle as a codec (10 s in all) before its
/// departures count as speech: the signalling channels pass through the
/// idle-codec reading only now and then.
const CODEC_PROOF: usize = 80_000;

/// What a channel is doing over the last half second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelState {
    #[default]
    Unknown,
    /// Always 0 or always 1.
    Fixed(u8),
    /// A 4 ms pattern over and over.
    Pattern,
    /// A 20 ms frame over and over: an idle codec channel.
    IdleCodec,
    /// Mostly repeating, with some change (signalling, a slow channel).
    Varying,
    /// Changing from one 20 ms frame to the next: traffic.
    Active,
}

impl ChannelState {
    pub fn label(&self) -> &'static str {
        match self {
            ChannelState::Unknown => "—",
            ChannelState::Fixed(0) => "fixed 0",
            ChannelState::Fixed(_) => "fixed 1",
            ChannelState::Pattern => "4 ms pattern",
            ChannelState::IdleCodec => "idle codec",
            ChannelState::Varying => "varying",
            ChannelState::Active => "ACTIVE",
        }
    }
}

/// One channel's meter: the share of bits differing from 20 ms and 4 ms
/// before, and of ones, over the window.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ChannelView {
    pub state: ChannelState,
    pub change_20ms: f32,
    pub change_4ms: f32,
    pub ones: f32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TdmStats {
    pub locked: bool,
    pub frames: u64,
    /// Alignment bits wrong while locked; times lock was lost.
    pub faw_errors: u64,
    pub losses: u64,
    /// The last multiframe's data bits from the alignment channel (35 bits:
    /// 7 a 28 ms cycle, five cycles), oldest first.
    pub side_data: Vec<u8>,
    pub channels: [ChannelView; CHANNELS],
}

/// A channel's recent bits and running counts.
#[derive(Clone)]
struct Meter {
    hist: Vec<u8>,
    at: usize,
    filled: usize,
    /// Per bit in the window: (differs at 20 ms, differs at 4 ms, is one).
    flags: Vec<u8>,
    sums: [u32; 3],
    /// Bits spent idle as a codec, and where the last view was; past
    /// `CODEC_PROOF`, the channel's frame departing is speech.
    idle_bits: usize,
    viewed_at: usize,
}

impl Meter {
    fn new() -> Self {
        Meter {
            hist: vec![0; CODEC_LAG],
            at: 0,
            filled: 0,
            flags: vec![0; WINDOW],
            sums: [0; 3],
            idle_bits: 0,
            viewed_at: 0,
        }
    }

    fn push(&mut self, b: u8, k: usize) {
        let back = |lag: usize| self.hist[(self.at + CODEC_LAG - lag) % CODEC_LAG];
        let f = u8::from(self.filled >= CODEC_LAG && back(CODEC_LAG) != b)
            | u8::from(self.filled >= PATTERN_LAG && back(PATTERN_LAG) != b) << 1
            | b << 2;
        let slot = k % WINDOW;
        let old = self.flags[slot];
        for (i, s) in self.sums.iter_mut().enumerate() {
            *s = *s + u32::from((f >> i) & 1) - u32::from((old >> i) & 1);
        }
        self.flags[slot] = f;
        self.hist[self.at] = b;
        self.at = (self.at + 1) % CODEC_LAG;
        self.filled += 1;
    }

    fn view(&mut self) -> ChannelView {
        let n = self.filled.clamp(1, WINDOW) as f32;
        let [c20, c4, ones] = self.sums.map(|s| s as f32 / n);
        let state = if self.filled < WINDOW {
            ChannelState::Unknown
        } else if ones < 0.002 || ones > 0.998 {
            ChannelState::Fixed(u8::from(ones > 0.5))
        } else if c4 < 0.01 {
            ChannelState::Pattern
        } else if c20 < IDLE_CHANGE {
            self.idle_bits += self.filled - self.viewed_at;
            ChannelState::IdleCodec
        } else if c20 > ACTIVE_CHANGE
            || (self.idle_bits >= CODEC_PROOF && c20 > CODEC_ACTIVE_CHANGE)
        {
            ChannelState::Active
        } else {
            ChannelState::Varying
        };
        self.viewed_at = self.filled;
        ChannelView {
            state,
            change_20ms: c20,
            change_4ms: c4,
            ones,
        }
    }
}

pub struct TdmRx {
    held: Vec<u8>,
    /// Locked: where the next frame starts in `held`, whether its
    /// alignment bit is a word bit, and which word bit comes next.
    lock: Option<(usize, bool, usize)>,
    recent: u16,
    side: Vec<u8>,
    meters: Vec<Meter>,
    bits_in: usize,
    pub stats: TdmStats,
    /// Keep every aligned frame in `frames_out` (for analysis).
    pub keep_frames: bool,
    /// The frames kept: 257 bits (0/1) each, the alignment bit first.
    pub frames_out: Vec<u8>,
}

impl Default for TdmRx {
    fn default() -> Self {
        Self::new()
    }
}

impl TdmRx {
    pub fn new() -> Self {
        TdmRx {
            held: Vec::new(),
            lock: None,
            recent: 0,
            side: Vec::new(),
            meters: vec![Meter::new(); CHANNELS],
            bits_in: 0,
            stats: TdmStats::default(),
            keep_frames: false,
            frames_out: Vec::new(),
        }
    }

    /// Bits (0/1) of the 128.5 kbit/s stream.
    pub fn push(&mut self, bits: &[u8]) {
        self.held.extend_from_slice(bits);
        loop {
            if self.lock.is_none() && !self.search() {
                break;
            }
            let Some((at, is_faw, k)) = self.lock else {
                break;
            };
            if at + FRAME > self.held.len() {
                break;
            }
            self.frame(at, is_faw, k);
        }
        // Keep what the next frame or search needs.
        let keep_from = match self.lock {
            Some((at, ..)) => at,
            None => self.held.len().saturating_sub(SEARCH_FRAMES * FRAME),
        };
        if keep_from > 0 {
            self.held.drain(..keep_from);
            if let Some((at, ..)) = &mut self.lock {
                *at -= keep_from;
            }
        }
        for (v, m) in self.stats.channels.iter_mut().zip(&mut self.meters) {
            *v = m.view();
        }
    }

    /// Look for the alignment word at every bit phase, in either parity
    /// of frame and any of its rotations.
    fn search(&mut self) -> bool {
        if self.held.len() < SEARCH_FRAMES * FRAME {
            return false;
        }
        let n = FAW.len();
        let mut best = (0usize, 0usize, 0usize, 0usize);
        for p in 0..FRAME {
            for par in 0..2 {
                for r in 0..n {
                    let hits = (0..SEARCH_FRAMES / 2)
                        .filter(|&i| self.held[p + (par + 2 * i) * FRAME] == FAW[(i + r) % n])
                        .count();
                    if hits > best.0 {
                        best = (hits, p, par, r);
                    }
                }
            }
        }
        let (hits, p, par, r) = best;
        if hits * 20 < (SEARCH_FRAMES / 2) * 19 {
            // Under 95 % of the word: not here yet.
            let drop = self.held.len() - (SEARCH_FRAMES - 2) * FRAME;
            self.held.drain(..drop);
            return false;
        }
        // Frame `par` (0 or 1) after `p` carries word bit `r`.
        self.lock = Some((p, par == 0, r));
        self.recent = 0;
        self.stats.locked = true;
        true
    }

    /// One frame at `at`: its alignment bit (word bit `k` if `is_faw`,
    /// else a data bit), then the payload into the channel meters.
    fn frame(&mut self, at: usize, is_faw: bool, k: usize) {
        let side = self.held[at];
        let next = if is_faw {
            let wrong = side != FAW[k];
            self.recent = (self.recent << 1) | u16::from(wrong);
            if wrong {
                self.stats.faw_errors += 1;
            }
            (false, (k + 1) % FAW.len())
        } else {
            self.side.push(side);
            if self.side.len() > 5 * FAW.len() {
                self.side.remove(0);
            }
            (true, k)
        };
        self.stats.side_data.clone_from(&self.side);
        // The last 14 word bits checked.
        if (self.recent & 0x3FFF).count_ones() >= LOSE {
            self.lock = None;
            self.stats.locked = false;
            self.stats.losses += 1;
            self.held.drain(..at + 1);
            return;
        }
        if self.keep_frames {
            self.frames_out
                .extend_from_slice(&self.held[at..at + FRAME]);
        }
        let payload = &self.held[at + 1..at + FRAME];
        for w in 0..WORDS {
            for (c, m) in self.meters.iter_mut().enumerate() {
                m.push(payload[w * CHANNELS + c], self.bits_in);
            }
            self.bits_in += 1;
        }
        self.stats.frames += 1;
        self.lock = Some((at + FRAME, next.0, next.1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frames as the multiplex sends them: channel 0 an idle codec frame
    /// (160 bits again and again), channel 3 a 4 ms pattern, channel 12
    /// always 0, channel 15 random (traffic), the rest a mix.
    fn stream(frames: usize, offset: usize) -> Vec<u8> {
        let mut s = 0x1234_5678u32;
        let mut rnd = move || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s & 1) as u8
        };
        let codec: Vec<u8> = (0..160).map(|i| ((i * 7 + i / 3) % 5 < 2) as u8).collect();
        let pattern: Vec<u8> = (0..32).map(|i| (i % 9 < 4) as u8).collect();
        let mut out: Vec<u8> = (0..offset).map(|_| rnd()).collect();
        let mut k = 0usize;
        for f in 0..frames {
            out.push(if f % 2 == 0 { FAW[(f / 2) % 7] } else { rnd() });
            for _ in 0..WORDS {
                for c in 0..CHANNELS {
                    out.push(match c {
                        0 => codec[k % 160],
                        3 => pattern[k % 32],
                        12 => 0,
                        15 => rnd(),
                        _ => codec[(k + 37 * c) % 160],
                    });
                }
                k += 1;
            }
        }
        out
    }

    #[test]
    fn locks_on_the_alignment_word_and_meters_channels() {
        let bits = stream(1500, 123);
        let mut rx = TdmRx::new();
        for chunk in bits.chunks(5000) {
            rx.push(chunk);
        }
        let s = &rx.stats;
        assert!(s.locked, "{s:?}");
        assert_eq!(s.faw_errors, 0);
        assert!(s.frames > 1300, "{}", s.frames);
        let st = |c: usize| s.channels[c].state;
        assert_eq!(st(0), ChannelState::IdleCodec);
        assert_eq!(st(3), ChannelState::Pattern);
        assert_eq!(st(12), ChannelState::Fixed(0));
        assert_eq!(st(15), ChannelState::Active);
        assert_eq!(s.side_data.len(), 35);
    }

    /// A codec channel as the multiplex carries it: 4 ms subframes
    /// `D(n) D(n−1) S S`, MSB first; `data(n)` gives D(n).
    fn codec_channel(subframes: usize, data: impl Fn(usize) -> u8, s: u8) -> Vec<u8> {
        let mut out = Vec::new();
        for n in 0..subframes {
            let prev = if n == 0 { 0xFF } else { data(n - 1) };
            for o in [data(n), prev, s, s] {
                out.extend((0..8).rev().map(|k| (o >> k) & 1));
            }
        }
        out
    }

    #[test]
    fn speech_in_an_idle_codec_channel_is_active() {
        // Channel 0: 12 s idle (the five-octet silence frame), then 1 s of
        // speech-like data: D octets with a random high nibble (codec
        // frames keep some bits steady) — about an eighth of the bits
        // change at 20 ms, half the generic threshold.
        let idle = [0xFF, 0xFE, 0xF3, 0xCE, 0xBB];
        let mut x = 0x9E37_79B9u32;
        let speech: Vec<u8> = (0..250)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x as u8 & 0xF0) | 0x0B
            })
            .collect();
        let quiet = codec_channel(3000, |n| idle[n % 5], 0x03);
        let talk = codec_channel(250, |n| speech[n], 0x03);
        let ch0: Vec<u8> = quiet.into_iter().chain(talk).collect();
        // Into frames: channel 0 is bit 0 of each word; the rest fixed 0.
        let frames = ch0.len() / WORDS;
        let mut bits = Vec::new();
        for f in 0..frames {
            bits.push(if f % 2 == 0 { FAW[(f / 2) % 7] } else { 0 });
            for w in 0..WORDS {
                bits.push(ch0[f * WORDS + w]);
                bits.extend(std::iter::repeat_n(0, CHANNELS - 1));
            }
        }
        let mut rx = TdmRx::new();
        let mut seen = Vec::new();
        for chunk in bits.chunks(257 * 50) {
            rx.push(chunk);
            seen.push(rx.stats.channels[0].state);
        }
        assert!(seen.contains(&ChannelState::IdleCodec), "{seen:?}");
        let last = rx.stats.channels[0];
        assert_eq!(last.state, ChannelState::Active, "{last:?}");
        assert!(last.change_20ms < ACTIVE_CHANGE, "{last:?}");
    }

    #[test]
    fn noise_does_not_lock() {
        let mut s = 99u32;
        let bits: Vec<u8> = (0..400_000)
            .map(|_| {
                s = s.wrapping_mul(1_103_515_245).wrapping_add(12345);
                ((s >> 16) & 1) as u8
            })
            .collect();
        let mut rx = TdmRx::new();
        rx.push(&bits);
        assert!(!rx.stats.locked);
    }
}
