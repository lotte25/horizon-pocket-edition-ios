//! Voice chat's end of a connection in a player's game (`horizon_voice::wire`).
//!
//! What the host sends is taken off the connection's own thread as it arrives, not when the
//! game next draws a frame: a voice goes straight to the engine the sound output pulls from
//! ([`Link::attach`]), and the rest (the welcome, the list of players) waits for the game
//! ([`Link::take`]). What this game sends (its state, its microphone's frames) goes out from
//! whichever thread has it ([`Sender`]).
use horizon_protocol::{
    connection::ConnWriter,
    packets::{configuration::CustomPayload, play},
};
use horizon_voice::{
    engine::Playout,
    wire::{CHANNEL, Down, Entry, VERSION},
};
use std::{
    cell::RefCell,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// What the host said that the game acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Told {
    /// This player is in voice chat's reach, under this number.
    Welcome { id: u8 },
    /// Everyone in reach, as of now.
    Roster(Vec<Entry>),
}

/// The most messages kept for the game between two of its frames (a list replaces the one
/// before it, so this is never near).
const MOST: usize = 64;

/// One connection's voice chat.
#[derive(Default)]
pub struct Link {
    /// The host carries voices, in the version this game speaks.
    offered: AtomicBool,
    /// Where the others' voices go, while voice chat is on here.
    playout: Mutex<Option<Arc<Playout>>>,
    told: Mutex<Vec<Told>>,
    /// Raw inbound Simple Voice Chat (`voicechat:*`) TCP plugin messages waiting for the game.
    svc_in: Mutex<Vec<(String, Vec<u8>)>>,
}

thread_local! {
    /// The link of the connection this thread reads (the connection's own thread).
    static HERE: RefCell<Option<Arc<Link>>> = const { RefCell::new(None) };
}

/// This thread reads `link`'s connection from now on.
pub fn read_here(link: Arc<Link>) {
    HERE.with(|here| *here.borrow_mut() = Some(link));
}

/// The configuration's custom payload on the voice channel: the host's offer.
pub fn offered(data: &[u8]) {
    HERE.with(|here| {
        if let Some(link) = here.borrow().as_ref() {
            link.offered
                .store(data.first() == Some(&VERSION), Ordering::SeqCst);
        }
    });
}

/// A play custom payload on the voice channel, as it arrives.
pub fn received(data: &[u8]) {
    HERE.with(|here| {
        if let Some(link) = here.borrow().as_ref() {
            link.receive(data);
        }
    });
}

/// A play custom payload on a Simple Voice Chat (`voicechat:*`) channel, as it arrives.
pub fn svc_received(channel: &str, data: Vec<u8>) {
    HERE.with(|here| {
        if let Some(link) = here.borrow().as_ref() {
            link.push_svc(channel, data);
        }
    });
}

impl Link {
    /// The host carries voices this game can speak.
    pub fn offered(&self) -> bool {
        self.offered.load(Ordering::SeqCst)
    }

    /// The others' voices go to `playout` from now on (None: nowhere, voice chat is off).
    pub fn attach(&self, playout: Option<Arc<Playout>>) {
        *self.playout.lock().unwrap_or_else(|e| e.into_inner()) = playout;
    }

    /// What the host said since the last call, in order.
    pub fn take(&self) -> Vec<Told> {
        std::mem::take(&mut *self.told.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Simple Voice Chat (`voicechat:*`) TCP payloads received since the last call.
    pub fn take_svc(&self) -> Vec<(String, Vec<u8>)> {
        std::mem::take(&mut *self.svc_in.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn push_svc(&self, channel: &str, data: Vec<u8>) {
        let mut waiting = self.svc_in.lock().unwrap_or_else(|e| e.into_inner());
        if waiting.len() < 256 {
            waiting.push((channel.to_owned(), data));
        }
    }

    fn receive(&self, data: &[u8]) {
        match Down::decode(data) {
            Some(Down::Frame {
                id,
                seq,
                flags,
                opus,
            }) => {
                if let Some(playout) = self
                    .playout
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    playout.push(id, seq, flags, opus);
                }
            }
            Some(Down::Welcome { version, id }) if version == VERSION => {
                self.tell(Told::Welcome { id });
            }
            Some(Down::Roster { entries }) => self.tell(Told::Roster(entries)),
            _ => {}
        }
    }

    fn tell(&self, told: Told) {
        let mut waiting = self.told.lock().unwrap_or_else(|e| e.into_inner());
        // Only the newest list matters.
        if matches!(told, Told::Roster(_)) {
            waiting.retain(|t| !matches!(t, Told::Roster(_)));
        }
        if waiting.len() < MOST {
            waiting.push(told);
        }
    }
}

/// Sends this game's voice messages on its connection, from any thread.
#[derive(Clone)]
pub struct Sender {
    pub(crate) writer: Arc<Mutex<Option<ConnWriter>>>,
    pub(crate) configuring: Arc<AtomicBool>,
}

impl Sender {
    /// Sends one message (`horizon_voice::wire::Up`, encoded). Dropped when the connection is
    /// gone or back in configuration, as any play packet is.
    pub fn send(&self, data: Vec<u8>) {
        self.send_custom(CHANNEL, data);
    }

    /// Sends a play `CustomPayload` on an arbitrary plugin channel (e.g. `voicechat:*`).
    pub fn send_custom(&self, channel: &str, data: Vec<u8>) {
        let packet = play::Serverbound::CustomPayload(CustomPayload {
            channel: channel.into(),
            data,
        });
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        if self.configuring.load(Ordering::Relaxed) {
            return;
        }
        if let Some(w) = writer.as_mut()
            && w.send(&packet.encode()).is_err()
        {
            w.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_voice::wire::START;

    #[test]
    fn a_voice_goes_to_the_engine_and_the_rest_waits_for_the_game() {
        let link = Arc::new(Link::default());
        assert!(!link.offered());
        // Nothing is read on this thread yet: what comes is nobody's.
        offered(&[VERSION]);
        assert!(!link.offered());
        read_here(link.clone());
        // Another version of the messages isn't an offer this game can take.
        offered(&[VERSION + 1]);
        assert!(!link.offered());
        offered(&[]);
        assert!(!link.offered());
        offered(&[VERSION]);
        assert!(link.offered());
        let entry = |id: u8| Entry {
            id,
            uuid: u128::from(id),
            name: format!("P{id}"),
            enabled: true,
            mic_live: false,
        };
        received(
            &Down::Welcome {
                version: VERSION,
                id: 3,
            }
            .encode(),
        );
        received(
            &Down::Welcome {
                version: VERSION + 1,
                id: 4,
            }
            .encode(),
        );
        received(
            &Down::Roster {
                entries: vec![entry(1)],
            }
            .encode(),
        );
        received(
            &Down::Roster {
                entries: vec![entry(1), entry(2)],
            }
            .encode(),
        );
        received(&[0xff, 1, 2]);
        received(&[]);
        assert_eq!(
            link.take(),
            [
                Told::Welcome { id: 3 },
                Told::Roster(vec![entry(1), entry(2)])
            ]
        );
        assert!(link.take().is_empty());
        // A voice with voice chat off goes nowhere; on, to the engine.
        let frame = Down::Frame {
            id: 1,
            seq: 0,
            flags: START,
            opus: &[1, 2, 3],
        }
        .encode();
        received(&frame);
        let playout = Arc::new(Playout::default());
        link.attach(Some(playout.clone()));
        assert!(!playout.live());
        received(&frame);
        assert!(playout.live());
        assert!(link.take().is_empty());
        link.attach(None);
        playout.clear();
        received(&frame);
        assert!(!playout.live());
        // A flood of lists leaves one; of welcomes, no more than the game can take.
        for id in 0..200 {
            received(
                &Down::Welcome {
                    version: VERSION,
                    id,
                }
                .encode(),
            );
        }
        assert_eq!(link.take().len(), MOST);
        HERE.with(|here| *here.borrow_mut() = None);
    }
}
