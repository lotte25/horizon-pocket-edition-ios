//! Voice chat between the players of a world.
//!
//! No game code and nothing unsafe: the game gives it microphone samples and asks it for the
//! others' voices ([`engine`]); in between are the codec ([`codec`]), the messages ([`wire`]),
//! the microphone's clean-up ([`dsp`]), each speaker's buffer ([`jitter`]) and the change of
//! rate ([`resample`]). What a player chose is kept by [`settings`].
//!
//! One voice is 48 kHz mono in frames of 20 ms ([`FRAME`] samples), whatever the microphone
//! and the sound output run at.
#![deny(unsafe_code)]

pub mod codec;
pub mod dsp;
pub mod engine;
pub mod jitter;
pub mod resample;
pub mod settings;
pub mod svc;
pub mod wire;

/// The rate voices are coded at.
pub const RATE: u32 = 48_000;
/// Samples in one frame: 20 ms.
pub const FRAME: usize = 960;
