//! The two ends of voice chat in one game: the others' voices on their way to the sound
//! output ([`Playout`]), and this player's microphone on its way to the connection
//! ([`CapturePipe`]).
//!
//! Neither runs anything while there is nothing to do: voices are decoded when the sound
//! output asks for them (and it is told at once when there are none), and the microphone's
//! thread exists only while the microphone is live.
use crate::{
    FRAME, RATE,
    codec::{Encoder, MAX_PACKET},
    dsp::{Chain, Verdict, rms, soft_limit},
    jitter::Jitter,
    resample::Resampler,
    wire::{END, START, Up},
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// The loudest a player's volume goes: 200 %.
pub const MAX_VOLUME: f32 = 2.0;

/// What a volume (0 to [`MAX_VOLUME`], 1 as it comes) multiplies a voice by: its square, so
/// the slider's halves feel like halves (50 % is a quarter of the power, 200 % four times).
pub fn gain_of(volume: f32) -> f32 {
    let volume = if volume.is_finite() { volume } else { 1.0 };
    let volume = volume.clamp(0.0, MAX_VOLUME);
    volume * volume
}

struct Speaker {
    jitter: Jitter,
    gain: f32,
    spatial_gain: f32,
    pan: f32,
    muted: bool,
    level: f32,
    talking: bool,
}

/// One speaker, for the screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Heard {
    pub id: u8,
    /// How loud they come out now (0 to 1).
    pub level: f32,
    pub talking: bool,
    /// How far behind their voice plays, from its arrival here.
    pub delay_ms: u32,
}

struct Inner {
    speakers: BTreeMap<u8, Speaker>,
    /// One speaker's voice, and everyone's together (left & right), at 48 kHz.
    voice: Vec<f32>,
    mix_l: Vec<f32>,
    mix_r: Vec<f32>,
    /// To the sound output's rate, and what is made and not asked for yet.
    resampler_l: Resampler,
    resampler_r: Resampler,
    fresh_l: Vec<f32>,
    fresh_r: Vec<f32>,
    ready_l: VecDeque<f32>,
    ready_r: VecDeque<f32>,
}

/// The others' voices, from the connection to the sound output.
pub struct Playout {
    inner: Mutex<Inner>,
    /// Something may sound: frames came that haven't played out.
    live: AtomicBool,
    master: AtomicU32,
    /// How loud the voices last came out, for the microphone's echo guard.
    far: AtomicU32,
    started: Instant,
}

impl Default for Playout {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                speakers: BTreeMap::new(),
                voice: Vec::new(),
                mix_l: Vec::new(),
                mix_r: Vec::new(),
                resampler_l: Resampler::new(RATE, RATE),
                resampler_r: Resampler::new(RATE, RATE),
                fresh_l: Vec::new(),
                fresh_r: Vec::new(),
                ready_l: VecDeque::new(),
                ready_r: VecDeque::new(),
            }),
            live: AtomicBool::new(false),
            master: AtomicU32::new(1f32.to_bits()),
            far: AtomicU32::new(0f32.to_bits()),
            started: Instant::now(),
        }
    }
}

impl Playout {
    /// A frame of player `id`'s voice, as it arrives.
    pub fn push(&self, id: u8, seq: u16, flags: u8, opus: &[u8]) {
        self.push_at(id, seq, flags, opus, self.started.elapsed());
    }

    /// As [`Self::push`], arriving at `now` (a clock that only goes forward).
    pub fn push_at(&self, id: u8, seq: u16, flags: u8, opus: &[u8], now: Duration) {
        let mut inner = locked(&self.inner);
        let speaker = match inner.speakers.entry(id) {
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::btree_map::Entry::Vacant(e) => {
                // Not on the list yet (the list is on its way): as they come, until it says.
                let Ok(jitter) = Jitter::new() else { return };
                e.insert(Speaker {
                    jitter,
                    gain: 1.0,
                    spatial_gain: 1.0,
                    pan: 0.0,
                    muted: false,
                    level: 0.0,
                    talking: false,
                })
            }
        };
        if speaker.muted {
            return;
        }
        speaker.jitter.push(seq, flags, opus, now);
        self.live.store(true, Ordering::SeqCst);
    }

    /// Player `id` is heard at `volume` (0 to [`MAX_VOLUME`]), or not at all.
    pub fn set_speaker(&self, id: u8, volume: f32, muted: bool) {
        let mut inner = locked(&self.inner);
        let muted = muted || gain_of(volume) == 0.0;
        if let Some(speaker) = inner.speakers.get_mut(&id) {
            speaker.gain = gain_of(volume);
            if muted && !speaker.muted {
                speaker.jitter.clear();
                speaker.level = 0.0;
                speaker.talking = false;
            }
            speaker.muted = muted;
        } else if let Ok(jitter) = Jitter::new() {
            inner.speakers.insert(
                id,
                Speaker {
                    jitter,
                    gain: gain_of(volume),
                    spatial_gain: 1.0,
                    pan: 0.0,
                    muted,
                    level: 0.0,
                    talking: false,
                },
            );
        }
    }

    /// Sets proximity attenuation (`spatial_gain` in `0.0..=1.0`) and stereo pan (`pan` in
    /// `-1.0..=1.0`, left to right) for speaker `id`.
    pub fn set_spatial(&self, id: u8, spatial_gain: f32, pan: f32) {
        let mut inner = locked(&self.inner);
        if let Some(speaker) = inner.speakers.get_mut(&id) {
            speaker.spatial_gain = if spatial_gain.is_finite() {
                spatial_gain.clamp(0.0, 1.0)
            } else {
                1.0
            };
            speaker.pan = if pan.is_finite() {
                pan.clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
    }

    /// Only the players in `ids` are left (the others went).
    pub fn keep(&self, ids: &[u8]) {
        locked(&self.inner)
            .speakers
            .retain(|id, _| ids.contains(id));
    }

    /// Everyone's volume together (0 to [`MAX_VOLUME`]).
    pub fn set_master(&self, volume: f32) {
        self.master
            .store(gain_of(volume).to_bits(), Ordering::SeqCst);
    }

    /// Nobody is heard any more (voice chat went off, or the connection did).
    pub fn clear(&self) {
        let mut inner = locked(&self.inner);
        inner.speakers.clear();
        inner.ready_l.clear();
        inner.ready_r.clear();
        inner.resampler_l.reset();
        inner.resampler_r.reset();
        self.live.store(false, Ordering::SeqCst);
        self.far.store(0f32.to_bits(), Ordering::SeqCst);
    }

    /// Whether [`Self::render`] would add anything.
    pub fn live(&self) -> bool {
        self.live.load(Ordering::SeqCst)
    }

    /// How loud the voices last came out (0 to 1).
    pub fn far_level(&self) -> f32 {
        f32::from_bits(self.far.load(Ordering::SeqCst))
    }

    /// Each speaker now, for the screen.
    pub fn heard(&self) -> Vec<Heard> {
        locked(&self.inner)
            .speakers
            .iter()
            .map(|(id, s)| Heard {
                id: *id,
                level: s.level,
                talking: s.talking,
                delay_ms: s.jitter.delay_ms(),
            })
            .collect()
    }

    /// Adds the voices to `out`, the sound output's next samples (`channels` interleaved, at
    /// `rate`), and says whether there were any. Called by the sound output, at its pace.
    pub fn render(&self, out: &mut [f32], channels: usize, rate: u32) -> bool {
        if !self.live() || channels == 0 || rate == 0 {
            return false;
        }
        let master = f32::from_bits(self.master.load(Ordering::SeqCst));
        let mut guard = locked(&self.inner);
        let inner = &mut *guard;
        let frames = out.len() / channels;
        if inner.resampler_l.rates().1 != rate {
            inner.resampler_l = Resampler::new(RATE, rate);
            inner.resampler_r = Resampler::new(RATE, rate);
            inner.ready_l.clear();
            inner.ready_r.clear();
        }
        let mut power = 0.0f32;
        if inner.resampler_l.same() {
            inner.blend(frames, master);
            for (frame, (&l, &r)) in out
                .chunks_exact_mut(channels)
                .zip(inner.mix_l.iter().zip(&inner.mix_r))
            {
                let mono = 0.5 * (l + r);
                power += 0.5 * (l * l + r * r);
                if channels == 1 {
                    frame[0] += mono;
                } else {
                    frame[0] += l;
                    frame[1] += r;
                    for sample in &mut frame[2..] {
                        *sample += mono;
                    }
                }
            }
        } else {
            while inner.ready_l.len() < frames {
                inner.blend(FRAME, master);
                inner.fresh_l.clear();
                inner.fresh_r.clear();
                inner.resampler_l.process(&inner.mix_l, &mut inner.fresh_l);
                inner.resampler_r.process(&inner.mix_r, &mut inner.fresh_r);
                inner.ready_l.extend(inner.fresh_l.iter());
                inner.ready_r.extend(inner.fresh_r.iter());
            }
            for frame in out.chunks_exact_mut(channels) {
                let l = inner.ready_l.pop_front().unwrap_or(0.0);
                let r = inner.ready_r.pop_front().unwrap_or(0.0);
                let mono = 0.5 * (l + r);
                power += 0.5 * (l * l + r * r);
                if channels == 1 {
                    frame[0] += mono;
                } else {
                    frame[0] += l;
                    frame[1] += r;
                    for sample in &mut frame[2..] {
                        *sample += mono;
                    }
                }
            }
        }
        let level = (power / frames.max(1) as f32).sqrt();
        self.far.store(level.to_bits(), Ordering::SeqCst);
        // Everyone fell silent and all of it has played: nothing until the next frame comes.
        if inner.speakers.values().all(|s| s.muted || s.jitter.idle())
            && inner.ready_l.iter().all(|s| *s == 0.0)
            && inner.ready_r.iter().all(|s| *s == 0.0)
        {
            inner.ready_l.clear();
            inner.ready_r.clear();
            self.live.store(false, Ordering::SeqCst);
            self.far.store(0f32.to_bits(), Ordering::SeqCst);
            for speaker in inner.speakers.values_mut() {
                speaker.level = 0.0;
                speaker.talking = false;
            }
        }
        true
    }
}

impl Inner {
    /// The next `frames` samples of everyone together, at 48 kHz, into `mix_l` & `mix_r`.
    fn blend(&mut self, frames: usize, master: f32) {
        self.mix_l.clear();
        self.mix_l.resize(frames, 0.0);
        self.mix_r.clear();
        self.mix_r.resize(frames, 0.0);
        self.voice.clear();
        self.voice.resize(frames, 0.0);
        for speaker in self.speakers.values_mut() {
            if speaker.muted || speaker.jitter.idle() {
                speaker.level = 0.0;
                speaker.talking = false;
                continue;
            }
            speaker.jitter.pull(&mut self.voice);
            let effective = speaker.gain * speaker.spatial_gain;
            let pan = speaker.pan.clamp(-1.0, 1.0);
            let gain_l = (1.0 - pan.max(0.0)).sqrt();
            let gain_r = (1.0 + pan.min(0.0)).sqrt();
            // Each voice under full scale by itself, however far it is turned up: a loud
            // friend is loud, never broken.
            for ((sum_l, sum_r), &voice) in self
                .mix_l
                .iter_mut()
                .zip(self.mix_r.iter_mut())
                .zip(&self.voice)
            {
                let limited = soft_limit(voice * effective);
                *sum_l += limited * gain_l;
                *sum_r += limited * gain_r;
            }
            speaker.level = (speaker.jitter.level() * effective).min(1.0);
            speaker.talking = speaker.jitter.talking();
        }
        for (sum_l, sum_r) in self.mix_l.iter_mut().zip(self.mix_r.iter_mut()) {
            *sum_l = soft_limit(*sum_l * master);
            *sum_r = soft_limit(*sum_r * master);
        }
    }
}

/// Where the microphone's coded frames go: each is a whole [`Up::Frame`] message.
pub type Sink = Box<dyn Fn(Vec<u8>) + Send + Sync>;

/// Where raw coded Opus frames go (`(seq, flags, opus_bytes)` — `opus_bytes` is empty on `END`).
pub type RawFrameSink = Box<dyn Fn(u16, u8, &[u8]) + Send + Sync>;

/// How the microphone is treated.
#[derive(Clone, Default)]
pub struct CaptureOptions {
    /// The others' voices come out of this device's own speaker and nothing else keeps them
    /// out of the microphone: it is turned down while they are heard.
    pub echo_guard: Option<Arc<Playout>>,
    /// The system already levels the microphone (its voice processing): leave the level.
    pub level_as_is: bool,
    /// The noise filter the player turned on ([`crate::dsp::Denoise`]).
    pub noise_filter: bool,
}

struct Input {
    samples: VecDeque<f32>,
    rate: u32,
}

struct Shared {
    input: Mutex<Input>,
    wake: Condvar,
    stop: AtomicBool,
    level: AtomicU32,
    speaking: AtomicBool,
}

/// Frames from before the gate opened that are sent with the first: 40 ms, so a word's first
/// sound isn't cut.
const PRE_ROLL: usize = 2;

/// This player's microphone, from the device to the connection, while it is live. Dropping
/// it ends the stretch of speech under way and stops its thread.
pub struct CapturePipe {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl CapturePipe {
    /// Starts the microphone's thread; `sink` gets each coded [`Up::Frame`] message.
    pub fn start(sink: Sink, options: CaptureOptions) -> std::io::Result<Self> {
        Self::start_raw(
            Box::new(move |seq, flags, opus| {
                sink(Up::Frame { seq, flags, opus }.encode());
            }),
            options,
        )
    }

    /// Starts the microphone's thread; `sink` gets each raw coded Opus frame (`opus` is empty on `END`).
    pub fn start_raw(sink: RawFrameSink, options: CaptureOptions) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            input: Mutex::new(Input {
                samples: VecDeque::new(),
                rate: RATE,
            }),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            level: AtomicU32::new(0f32.to_bits()),
            speaking: AtomicBool::new(false),
        });
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("horizon-voice-mic".into())
                .spawn(move || run(&shared, &sink, &options))?
        };
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// The microphone's next samples (`channels` interleaved, at `rate`), as the device gives
    /// them. Called by the device, at its pace: it only copies.
    pub fn feed(&self, samples: &[f32], channels: usize, rate: u32) {
        let channels = channels.max(1);
        {
            let mut input = locked(&self.shared.input);
            if input.rate != rate {
                input.samples.clear();
                input.rate = rate.max(1);
            }
            for frame in samples.chunks_exact(channels) {
                input
                    .samples
                    .push_back(frame.iter().sum::<f32>() / channels as f32);
            }
            // The thread fell behind by a second (it never should): the oldest goes.
            let most = input.rate as usize;
            if input.samples.len() > most {
                let over = input.samples.len() - most;
                input.samples.drain(..over);
            }
        }
        self.shared.wake.notify_one();
    }

    /// How loud the microphone hears now (0 to 1), for the meter.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared.level.load(Ordering::SeqCst))
    }

    /// Speech is being sent now.
    pub fn speaking(&self) -> bool {
        self.shared.speaking.load(Ordering::SeqCst)
    }
}

impl Drop for CapturePipe {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The microphone's thread: clean, find the speech, code it, send it.
fn run(shared: &Shared, sink: &RawFrameSink, options: &CaptureOptions) {
    let Ok(mut encoder) = Encoder::new() else {
        return;
    };
    let mut chain = Chain::new(options.level_as_is, options.noise_filter);
    let mut resampler = Resampler::new(RATE, RATE);
    let mut raw: Vec<f32> = Vec::new();
    let mut pending: Vec<f32> = Vec::new();
    let mut before: VecDeque<[f32; FRAME]> = VecDeque::with_capacity(PRE_ROLL + 1);
    let mut talking = false;
    let mut seq = 0u16;
    let send = |encoder: &mut Encoder, frame: &[f32; FRAME], flags: u8, seq: &mut u16| {
        let mut packet = [0u8; MAX_PACKET];
        if let Ok(n) = encoder.encode(frame, &mut packet) {
            sink(*seq, flags, &packet[..n]);
            *seq = seq.wrapping_add(1);
        }
    };
    loop {
        let rate = {
            let mut input = locked(&shared.input);
            while input.samples.is_empty() && !shared.stop.load(Ordering::SeqCst) {
                input = shared.wake.wait(input).unwrap_or_else(|e| e.into_inner());
            }
            if shared.stop.load(Ordering::SeqCst) {
                break;
            }
            raw.clear();
            raw.extend(input.samples.drain(..));
            input.rate
        };
        if resampler.rates().0 != rate {
            resampler = Resampler::new(rate, RATE);
            pending.clear();
        }
        resampler.process(&raw, &mut pending);
        let mut used = 0;
        while pending.len() - used >= FRAME {
            let mut frame = [0.0; FRAME];
            frame.copy_from_slice(&pending[used..used + FRAME]);
            used += FRAME;
            let far = options.echo_guard.as_ref().map(|p| p.far_level());
            let verdict = chain.process(&mut frame, far);
            shared
                .level
                .store(rms(&frame).min(1.0).to_bits(), Ordering::SeqCst);
            match verdict {
                Verdict::Speech => {
                    let mut flags = 0;
                    if !talking {
                        talking = true;
                        shared.speaking.store(true, Ordering::SeqCst);
                        let _ = encoder.reset();
                        flags = START;
                        // The moment before the gate opened, at the level the speech is at.
                        let gain = chain.gain();
                        for mut old in before.drain(..) {
                            for s in &mut old {
                                *s = soft_limit(*s * gain);
                            }
                            send(&mut encoder, &old, flags, &mut seq);
                            flags = 0;
                        }
                    }
                    send(&mut encoder, &frame, flags, &mut seq);
                }
                Verdict::End => {
                    talking = false;
                    shared.speaking.store(false, Ordering::SeqCst);
                    sink(seq, END, &[]);
                    seq = seq.wrapping_add(1);
                }
                Verdict::Silent => {
                    if before.len() == PRE_ROLL {
                        before.pop_front();
                    }
                    before.push_back(frame);
                }
            }
        }
        pending.drain(..used);
    }
    // The microphone went off in the middle of a word: the listeners are told it ended.
    if talking {
        sink(seq, END, &[]);
    }
    shared.speaking.store(false, Ordering::SeqCst);
    shared.level.store(0f32.to_bits(), Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{
        Decoder,
        tests::{amplitude_at, tone},
    };

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// `frames` coded frames of a tone, the first a start.
    fn spoken(hz: f32, level: f32, frames: usize) -> Vec<(u8, Vec<u8>)> {
        let mut encoder = Encoder::new().unwrap();
        tone(hz, level, frames)
            .iter()
            .enumerate()
            .map(|(i, frame)| {
                let mut packet = [0u8; MAX_PACKET];
                let n = encoder.encode(frame, &mut packet).unwrap();
                (if i == 0 { START } else { 0 }, packet[..n].to_vec())
            })
            .collect()
    }

    /// Plays `voices` (id, frames) through `playout` as a steady connection would bring
    /// them, into an output of `channels` at `rate`, and returns the first channel.
    fn play(
        playout: &Playout,
        voices: &[(u8, Vec<(u8, Vec<u8>)>)],
        channels: usize,
        rate: u32,
    ) -> Vec<f32> {
        let frames = voices.iter().map(|(_, v)| v.len()).max().unwrap_or(0);
        let block = rate as usize / 50;
        let mut heard = Vec::new();
        for i in 0..frames + 30 {
            for (id, voice) in voices {
                if let Some((flags, opus)) = voice.get(i) {
                    playout.push_at(*id, i as u16, *flags, opus, ms(i as u64 * 20));
                } else if i == voice.len() {
                    playout.push_at(*id, i as u16, END, &[], ms(i as u64 * 20));
                }
            }
            let mut out = vec![0.0; block * channels];
            playout.render(&mut out, channels, rate);
            for frame in out.chunks_exact(channels) {
                // Every channel carries the same voice.
                assert!(frame.iter().all(|s| *s == frame[0]));
                heard.push(frame[0]);
            }
        }
        heard
    }

    fn amplitude(samples: &[f32], hz: f32, rate: u32) -> f32 {
        // amplitude_at counts at 48 kHz: the same tone at another rate is another pitch there.
        amplitude_at(samples, hz * RATE as f32 / rate as f32)
    }

    #[test]
    fn everyone_is_heard_together_each_at_their_volume() {
        let playout = Playout::default();
        let mut out = [0.25; 960];
        // Nothing came: the output is told so and left as it is.
        assert!(!playout.render(&mut out, 2, 48_000));
        assert_eq!(out, [0.25; 960]);
        playout.set_speaker(1, 1.0, false);
        playout.set_speaker(2, 0.5, false);
        let voices = [
            (1, spoken(300.0, 0.2, 100)),
            (2, spoken(700.0, 0.2, 100)),
            // Not on the list yet: heard as they come.
            (9, spoken(1_100.0, 0.2, 100)),
        ];
        let heard = play(&playout, &voices, 2, 48_000);
        let middle = &heard[48_000 / 2..48_000 * 3 / 2];
        assert!((amplitude_at(middle, 300.0) - 0.2).abs() < 0.03);
        // Half volume is a quarter.
        assert!((amplitude_at(middle, 700.0) - 0.05).abs() < 0.015);
        assert!((amplitude_at(middle, 1_100.0) - 0.2).abs() < 0.03);
        // All of it played out: nothing more to add, and nobody is talking.
        assert!(!playout.live());
        assert!(playout.heard().iter().all(|h| !h.talking && h.level == 0.0));
        assert_eq!(playout.far_level(), 0.0);
        let mut out = [0.0; 960];
        assert!(!playout.render(&mut out, 2, 48_000));
    }

    #[test]
    fn a_muted_player_isnt_heard_and_everyones_volume_is_one_knob() {
        let playout = Playout::default();
        playout.set_speaker(1, 1.0, true);
        playout.set_speaker(2, 1.0, false);
        // Turned all the way down is muted too.
        playout.set_speaker(3, 0.0, false);
        let voices = [
            (1, spoken(300.0, 0.3, 60)),
            (2, spoken(700.0, 0.3, 60)),
            (3, spoken(1_100.0, 0.3, 60)),
        ];
        let heard = play(&playout, &voices, 1, 48_000);
        let middle = &heard[48_000 / 4..48_000];
        assert!(amplitude_at(middle, 300.0) < 0.005);
        assert!(amplitude_at(middle, 1_100.0) < 0.005);
        assert!(amplitude_at(middle, 700.0) > 0.25);
        // Unmuted, they are heard again from their next word.
        playout.set_speaker(1, 1.0, false);
        playout.set_master(0.5);
        let heard = play(&playout, &voices[..1], 1, 48_000);
        assert!((amplitude_at(&heard[48_000 / 4..48_000], 300.0) - 0.075).abs() < 0.02);
        // Muted in the middle of a word: gone at once.
        playout.set_master(1.0);
        for (i, (flags, opus)) in voices[1].1.iter().take(20).enumerate() {
            playout.push_at(2, i as u16, *flags, opus, ms(i as u64 * 20));
        }
        playout.set_speaker(2, 1.0, true);
        let mut out = [0.0; 960];
        playout.render(&mut out, 1, 48_000);
        assert_eq!(out, [0.0; 960]);
        // Gone from the world: forgotten.
        playout.keep(&[1]);
        assert_eq!(playout.heard().len(), 1);
        playout.clear();
        assert!(playout.heard().is_empty() && !playout.live());
    }

    #[test]
    fn turned_all_the_way_up_and_all_talking_at_once_it_never_clips() {
        let playout = Playout::default();
        let voices: Vec<(u8, Vec<(u8, Vec<u8>)>)> = (0..8u8)
            .map(|id| {
                playout.set_speaker(id, MAX_VOLUME, false);
                (id, spoken(200.0 + f32::from(id) * 130.0, 0.9, 60))
            })
            .collect();
        playout.set_master(MAX_VOLUME);
        let heard = play(&playout, &voices, 2, 48_000);
        let peak = heard.iter().fold(0f32, |p, s| p.max(s.abs()));
        assert!(peak <= crate::dsp::CEILING, "over full scale: {peak}");
        assert!(peak > 0.8);
        assert_eq!(gain_of(f32::NAN), 1.0);
        assert_eq!(gain_of(9.0), 4.0);
        assert_eq!(gain_of(-1.0), 0.0);
    }

    #[test]
    fn the_voice_keeps_its_pitch_at_the_outputs_rate() {
        for rate in [44_100u32, 24_000, 96_000] {
            let playout = Playout::default();
            let heard = play(&playout, &[(1, spoken(500.0, 0.3, 100))], 2, rate);
            let middle = &heard[rate as usize / 2..rate as usize * 3 / 2];
            let level = amplitude(middle, 500.0, rate);
            assert!((level - 0.3).abs() < 0.04, "{rate}: {level}");
            assert!(!playout.live(), "{rate}: still live after it all played");
        }
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "waited for {what}"
            );
            std::thread::sleep(ms(5));
        }
    }

    #[test]
    fn the_microphone_sends_speech_whole_and_silence_not_at_all() {
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let pipe = {
            let sent = sent.clone();
            CapturePipe::start(
                Box::new(move |message| locked(&sent).push(message)),
                CaptureOptions::default(),
            )
            .unwrap()
        };
        // A second of a quiet room, as a stereo microphone at 44.1 kHz gives it.
        let quiet = vec![0.0001f32; 882];
        for _ in 0..100 {
            pipe.feed(&quiet, 2, 44_100);
        }
        let drained = || locked(&pipe.shared.input).samples.is_empty();
        wait_for("the room to be heard", drained);
        // Then a second of speech, then the room again.
        let speech: Vec<f32> = (0..44_100)
            .flat_map(|i| {
                let s = 0.2 * (std::f32::consts::TAU * 440.0 * i as f32 / 44_100.0).sin();
                [s, s]
            })
            .collect();
        for piece in speech.chunks(882) {
            pipe.feed(piece, 2, 44_100);
        }
        wait_for("the speech to be heard", drained);
        wait_for("the speech to be sent", || locked(&sent).len() >= 45);
        assert!(pipe.speaking() && pipe.level() > 0.02);
        for _ in 0..100 {
            pipe.feed(&quiet, 2, 44_100);
        }
        wait_for("the end of the speech", || !pipe.speaking());
        drop(pipe);
        let sent = locked(&sent);
        // One stretch: a start, frames in order, one end; about a second and its tail.
        let mut decoder = Decoder::new().unwrap();
        let mut heard = Vec::new();
        for (i, message) in sent.iter().enumerate() {
            let Some(Up::Frame { seq, flags, opus }) = Up::decode(message) else {
                panic!("not a frame");
            };
            assert_eq!(usize::from(seq), i);
            assert_eq!(flags & START != 0, i == 0);
            assert_eq!(flags & END != 0, i == sent.len() - 1);
            if !opus.is_empty() {
                let mut out = [0.0; FRAME];
                decoder.decode(opus, &mut out).unwrap();
                heard.extend_from_slice(&out);
            }
        }
        assert!((50..75).contains(&sent.len()), "{} messages", sent.len());
        // The tone, brought toward everyone's level.
        let level = amplitude_at(&heard[FRAME * 20..FRAME * 45], 440.0);
        assert!((0.1..0.3).contains(&level), "the speech at {level}");
    }

    #[test]
    fn a_microphone_turned_off_mid_word_ends_the_word() {
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let pipe = {
            let sent = sent.clone();
            CapturePipe::start(
                Box::new(move |message| locked(&sent).push(message)),
                CaptureOptions::default(),
            )
            .unwrap()
        };
        for frame in tone(300.0, 0.3, 20) {
            pipe.feed(&frame, 1, RATE);
        }
        wait_for("the word to start", || locked(&sent).len() >= 10);
        drop(pipe);
        let sent = locked(&sent);
        let Some(Up::Frame { flags, opus, .. }) = Up::decode(sent.last().unwrap()) else {
            panic!("not a frame");
        };
        assert!(flags & END != 0 && opus.is_empty());
    }

    #[test]
    fn on_the_speaker_the_others_voices_dont_come_back_through_the_microphone() {
        let playout = Arc::new(Playout::default());
        let sent: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let pipe = {
            let sent = sent.clone();
            CapturePipe::start(
                Box::new(move |message| locked(&sent).push(message)),
                CaptureOptions {
                    echo_guard: Some(playout.clone()),
                    level_as_is: false,
                    noise_filter: false,
                },
            )
            .unwrap()
        };
        // The others are heard (their level stands), and the microphone hears them quieter.
        playout.far.store(0.2f32.to_bits(), Ordering::SeqCst);
        for frame in tone(300.0, 0.04, 40) {
            pipe.feed(&frame, 1, RATE);
        }
        wait_for("the echo to be heard", || {
            locked(&pipe.shared.input).samples.is_empty()
        });
        std::thread::sleep(ms(100));
        assert!(locked(&sent).is_empty(), "the echo was sent");
        // They stopped: this player's own voice goes through.
        playout.far.store(0f32.to_bits(), Ordering::SeqCst);
        for frame in tone(300.0, 0.2, 45) {
            pipe.feed(&frame, 1, RATE);
        }
        wait_for("speech after the others stopped", || {
            !locked(&sent).is_empty()
        });
    }
}
