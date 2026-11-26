//! The codec: Opus, one frame of 20 ms of 48 kHz mono speech at a time.
//!
//! Full band (the whole voice, not a telephone's) at about 32 kbit/s, variable: a frame is
//! some 80 bytes, never more than [`MAX_PACKET`]. The connection it travels on loses nothing
//! and keeps order, so nothing is spent on recovering lost frames; a frame that comes too late
//! to be played is covered by the decoder ([`Decoder::conceal`]).
//!
//! The only file that names the codec's crate: another build of Opus changes this file alone.
use crate::{FRAME, RATE};

/// The most a coded frame takes on Starling's own wire: under the size the game's connection would compress.
pub const MAX_PACKET: usize = 220;
/// The largest incoming Opus frame accepted by the decoder (Simple Voice Chat servers in AUDIO
/// mode or higher bitrates can send frames up to 1024 bytes).
pub const MAX_DECODE_PACKET: usize = 1024;
/// Bits a second, on average.
pub const BITRATE: i32 = 32_000;
/// The encoder's effort, 0 to 10: past 5 the gain is small and the battery pays for it.
const COMPLEXITY: i32 = 5;

/// The codec refused: what it was doing, and what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the voice codec: {}", self.0)
    }
}

impl std::error::Error for Error {}

impl From<opus::Error> for Error {
    fn from(e: opus::Error) -> Self {
        Self(format!("{} ({})", e.description(), e.function()))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// The version of the Opus library built in, for the diagnostics.
pub fn version() -> &'static str {
    opus::version()
}

/// One speaker's encoder.
pub struct Encoder(opus::Encoder);

impl Encoder {
    pub fn new() -> Result<Self> {
        let mut encoder = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip)?;
        encoder.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
        encoder.set_vbr(true)?;
        encoder.set_complexity(COMPLEXITY)?;
        encoder.set_signal(opus::Signal::Voice)?;
        // Nothing is lost on the way, so nothing is spent on spare copies; and silence is not
        // sent at all (the gate decides), so the codec's own silence frames stay off.
        encoder.set_inband_fec(false)?;
        encoder.set_packet_loss_perc(0)?;
        encoder.set_dtx(false)?;
        Ok(Self(encoder))
    }

    /// Codes one frame into `out`, and returns how many bytes it took.
    pub fn encode(&mut self, frame: &[f32; FRAME], out: &mut [u8; MAX_PACKET]) -> Result<usize> {
        Ok(self.0.encode_float(frame, out)?)
    }

    /// Forgets what came before: the next frame starts a new stretch of speech.
    pub fn reset(&mut self) -> Result<()> {
        Ok(self.0.reset_state()?)
    }
}

/// One speaker's decoder.
pub struct Decoder(opus::Decoder);

impl Decoder {
    pub fn new() -> Result<Self> {
        Ok(Self(opus::Decoder::new(RATE, opus::Channels::Mono)?))
    }

    /// Decodes one coded frame into `out`. A packet that isn't one frame of 20 ms is refused.
    pub fn decode(&mut self, packet: &[u8], out: &mut [f32; FRAME]) -> Result<()> {
        if packet.is_empty() || packet.len() > MAX_DECODE_PACKET {
            return Err(Error(format!("a frame of {} bytes", packet.len())));
        }
        match self.0.decode_float(packet, out, false)? {
            FRAME => Ok(()),
            n => Err(Error(format!("a frame of {n} samples"))),
        }
    }

    /// Fills `out` with the decoder's guess at a frame that didn't come in time: the voice
    /// carried on and faded, not a hole.
    pub fn conceal(&mut self, out: &mut [f32; FRAME]) -> Result<()> {
        match self.0.decode_float(&[], out, false)? {
            FRAME => Ok(()),
            n => Err(Error(format!("a concealed frame of {n} samples"))),
        }
    }

    /// Forgets what came before: the next frame starts a new stretch of speech.
    pub fn reset(&mut self) -> Result<()> {
        Ok(self.0.reset_state()?)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// `frames` frames of a sine at `hz`, at `level` (0 to 1).
    pub(crate) fn tone(hz: f32, level: f32, frames: usize) -> Vec<[f32; FRAME]> {
        (0..frames)
            .map(|f| {
                let mut frame = [0.0; FRAME];
                for (i, sample) in frame.iter_mut().enumerate() {
                    let t = (f * FRAME + i) as f32 / RATE as f32;
                    *sample = level * (std::f32::consts::TAU * hz * t).sin();
                }
                frame
            })
            .collect()
    }

    /// How much of `samples` is the sine at `hz`: its amplitude, by correlation.
    pub(crate) fn amplitude_at(samples: &[f32], hz: f32) -> f32 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, s) in samples.iter().enumerate() {
            let phase = std::f64::consts::TAU * f64::from(hz) * i as f64 / f64::from(RATE);
            re += f64::from(*s) * phase.cos();
            im += f64::from(*s) * phase.sin();
        }
        (2.0 * (re * re + im * im).sqrt() / samples.len() as f64) as f32
    }

    pub(crate) fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32).sqrt()
    }

    #[test]
    fn a_tone_comes_back_as_itself() {
        let mut encoder = Encoder::new().unwrap();
        let mut decoder = Decoder::new().unwrap();
        let mut heard = Vec::new();
        let mut bytes = 0;
        let frames = tone(440.0, 0.5, 50);
        for frame in &frames {
            let mut packet = [0u8; MAX_PACKET];
            let n = encoder.encode(frame, &mut packet).unwrap();
            assert!(n > 0 && n <= MAX_PACKET);
            bytes += n;
            let mut out = [0.0; FRAME];
            decoder.decode(&packet[..n], &mut out).unwrap();
            heard.extend_from_slice(&out);
        }
        // The first frames are the codec settling; after that the tone is there, at its level,
        // and little else is.
        let settled = &heard[FRAME * 10..];
        let level = amplitude_at(settled, 440.0);
        assert!((0.4..0.6).contains(&level), "the tone at {level}");
        assert!(amplitude_at(settled, 1000.0) < 0.02);
        assert!((rms(settled) - 0.5 / 2f32.sqrt()).abs() < 0.05);
        // About the bitrate asked for: a second of it, within a half and twice.
        let bits = bytes * 8;
        assert!(
            (BITRATE as usize / 2..BITRATE as usize * 2).contains(&bits),
            "{bits} bits for a second"
        );
    }

    #[test]
    fn a_frame_that_didnt_come_is_covered_and_the_voice_goes_on() {
        let mut encoder = Encoder::new().unwrap();
        let mut decoder = Decoder::new().unwrap();
        let frames = tone(300.0, 0.4, 30);
        let mut heard = Vec::new();
        for (i, frame) in frames.iter().enumerate() {
            let mut packet = [0u8; MAX_PACKET];
            let n = encoder.encode(frame, &mut packet).unwrap();
            let mut out = [0.0; FRAME];
            if i == 20 {
                decoder.conceal(&mut out).unwrap();
                // Not silence, and not louder than the voice was.
                assert!(rms(&out) > 0.01 && out.iter().all(|s| s.abs() <= 1.0));
            } else {
                decoder.decode(&packet[..n], &mut out).unwrap();
            }
            heard.extend_from_slice(&out);
        }
        assert!(amplitude_at(&heard[FRAME * 22..], 300.0) > 0.25);
    }

    #[test]
    fn what_isnt_a_frame_is_refused_and_the_decoder_lives() {
        let mut decoder = Decoder::new().unwrap();
        let mut out = [0.0; FRAME];
        assert!(decoder.decode(&[], &mut out).is_err());
        assert!(decoder.decode(&[0u8; MAX_PACKET + 1], &mut out).is_err());
        // Bytes that are no Opus frame, or one of another length: an error or sound, never a
        // crash, and the decoder still works after.
        let mut seed = 7u32;
        for len in [1usize, 2, 3, 17, 80, MAX_PACKET] {
            let junk: Vec<u8> = (0..len)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (seed >> 24) as u8
                })
                .collect();
            let _ = decoder.decode(&junk, &mut out);
            assert!(out.iter().all(|s| s.is_finite()));
        }
        decoder.reset().unwrap();
        let mut encoder = Encoder::new().unwrap();
        let mut packet = [0u8; MAX_PACKET];
        let n = encoder
            .encode(&tone(440.0, 0.5, 1)[0], &mut packet)
            .unwrap();
        decoder.decode(&packet[..n], &mut out).unwrap();
        assert!(!version().is_empty());
    }

    #[test]
    fn silence_is_small_and_loud_speech_fits() {
        let mut encoder = Encoder::new().unwrap();
        let mut packet = [0u8; MAX_PACKET];
        let quiet = encoder.encode(&[0.0; FRAME], &mut packet).unwrap();
        assert!(quiet <= 80, "{quiet} bytes of silence");
        // The hardest thing to code, noise at full level, still fits one packet.
        let mut seed = 1u32;
        for _ in 0..50 {
            let mut frame = [0.0; FRAME];
            for s in &mut frame {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *s = (seed >> 8) as f32 / (1u32 << 23) as f32 - 1.0;
            }
            let n = encoder.encode(&frame, &mut packet).unwrap();
            assert!(n <= MAX_PACKET);
        }
    }
}
