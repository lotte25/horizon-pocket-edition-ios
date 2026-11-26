//! Voice chat in a shared world, in the window's game: whether it is on and whether the
//! microphone is, what the host is told, who is in reach, the lines in chat that say it is
//! there, and what the player chose for each of the others (`horizon-voice` codes, buffers
//! and mixes; `lan::voice` relays on the host; `voice_net` is this game's end of the
//! connection).
//!
//! It is off when a world is joined, every time, and so is the microphone when it is turned
//! on: nothing is heard, and nothing is said, that the player didn't ask for. Both stay as
//! they are while the world changes hosts (the connection is made again, the session isn't).
use super::{Game, Screen};
use horizon_client::{
    Result,
    extras::Corner,
    extras_hud::Corners,
    hud::{HudBatch, rgba},
    lang,
    svc_net::{SpatialSource, SvcUdpClient},
    text::Text,
    voice_net::{Link, Sender, Told},
    voice_screen::{self, Action, GroupField, GroupView, Held, Tab},
};
#[cfg(target_os = "ios")]
use horizon_gamecontroller::voice as phone;
use horizon_voice::{
    engine::{CaptureOptions, CapturePipe, Playout},
    settings::Settings,
    svc::{
        CH_CREATE_GROUP, CH_LEAVE_GROUP, CH_REQUEST_SECRET, CH_SECRET, CH_SET_GROUP, ClientGroup,
        GroupType, MAX_GROUP_NAME, MAX_GROUP_PASSWORD, PlayerState as SvcPlayerState, SecretInfo,
        SvcVersion, TcpIn, decode_secret, decode_tcp, encode_create_group, encode_join_group,
        encode_leave_group, encode_request_secret, encode_update_state, spatial_params,
    },
    wire::{END, Entry, Up, VERSION},
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use winit::keyboard::KeyCode;

fn tr(key: &str, english: &str) -> String {
    lang::get_or(&format!("horizon.voice.{key}"), english)
}

/// The colour of voice chat's lines in chat: gold, so they stand out from the rest.
const HINT: u32 = 0xFF_AA00;
/// The file the choices are kept in, in the player's folder.
const FILE: &str = "voice-chat.json";
/// A connection lost with the microphone live (the world changes hosts): live again when it
/// is back within this, muted otherwise.
const MIC_KEPT_FOR: Duration = Duration::from_secs(30);

/// The others' voices as the sound output asks for them.
struct Tap(Arc<Playout>);

impl horizon_client::audio::StreamSource for Tap {
    fn render(&self, out: &mut [f32], channels: usize, rate: u32) -> bool {
        self.0.render(out, channels, rate)
    }
}

/// The end-to-end checks' ears (`HORIZON_VOICE_TEST=1`, a game with no sound output): pulls
/// the voices as an output would and keeps what it heard.
struct Probe {
    heard: Arc<Mutex<Heard>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default, Clone, Copy)]
struct Heard {
    /// The level and pitch of the last 200 ms that had sound.
    level: f32,
    hz: f32,
    /// Blocks of 20 ms with sound, ever, and since when the last stretch of them runs.
    blocks: u64,
    quiet_blocks: u64,
}

impl Probe {
    fn start(playout: Arc<Playout>) -> Self {
        let heard: Arc<Mutex<Heard>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (heard, stop) = (heard.clone(), stop.clone());
            std::thread::Builder::new()
                .name("horizon-voice-probe".into())
                .spawn(move || {
                    let mut window: Vec<f32> = Vec::new();
                    let started = Instant::now();
                    let mut blocks = 0u32;
                    while !stop.load(Ordering::SeqCst) {
                        // An output's pace: 20 ms of sound every 20 ms.
                        let due = started + Duration::from_millis(20) * blocks;
                        if let Some(wait) = due.checked_duration_since(Instant::now()) {
                            std::thread::sleep(wait);
                        }
                        blocks += 1;
                        let mut block = [0.0f32; 960];
                        playout.render(&mut block, 1, 48_000);
                        let level = horizon_voice::dsp::rms(&block);
                        let mut heard = heard.lock().unwrap_or_else(|e| e.into_inner());
                        if level < 0.005 {
                            heard.quiet_blocks += 1;
                            window.clear();
                            continue;
                        }
                        heard.blocks += 1;
                        heard.quiet_blocks = 0;
                        window.extend_from_slice(&block);
                        if window.len() >= 9_600 {
                            // The pitch of a tone: its rising zero crossings a second.
                            let crossings = window
                                .windows(2)
                                .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
                                .count();
                            heard.hz = crossings as f32 * 48_000.0 / window.len() as f32;
                            heard.level = horizon_voice::dsp::rms(&window);
                            window.clear();
                        }
                    }
                })
                .ok()
        };
        Self {
            heard,
            stop,
            thread,
        }
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The end-to-end checks' mouth: a tone fed to the microphone's pipe at a microphone's pace.
struct Tone {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Tone {
    fn start(pipe: Arc<CapturePipe>, hz: f32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("horizon-voice-tone".into())
                .spawn(move || {
                    let started = Instant::now();
                    let mut blocks = 0u32;
                    let mut n = 0u64;
                    while !stop.load(Ordering::SeqCst) {
                        let due = started + Duration::from_millis(10) * blocks;
                        if let Some(wait) = due.checked_duration_since(Instant::now()) {
                            std::thread::sleep(wait);
                        }
                        blocks += 1;
                        let mut block = [0.0f32; 480];
                        for s in &mut block {
                            let t = n as f32 / 48_000.0;
                            *s = 0.2 * (std::f32::consts::TAU * hz * t).sin();
                            n += 1;
                        }
                        pipe.feed(&block, 1, 48_000);
                    }
                })
                .ok()
        };
        Self { stop, thread }
    }
}

impl Drop for Tone {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The device that feeds the microphone's pipe: held by the microphone's own thread for as
/// long as the microphone is live (dropping it stops the device).
#[cfg(target_os = "ios")]
type Device = phone::Capture;
#[cfg(not(target_os = "ios"))]
type Device = horizon_client::audio_capture::Capture;

/// The microphone while it is live: its pipe to the connection.
struct Mic {
    pipe: Arc<CapturePipe>,
    /// The checks' tone, when that is what feeds it.
    _tone: Option<Tone>,
    /// Where sound came out when it started: on another output the microphone is started
    /// again (the speaker needs the echo kept out, headphones don't).
    #[cfg(target_os = "ios")]
    output: Option<phone::Output>,
}

/// Makes a pipe to the connection for a microphone treated as the options say.
type MakePipe = Box<dyn Fn(CaptureOptions) -> std::result::Result<Arc<CapturePipe>, String> + Send>;

/// How the microphone is to be opened.
struct Plan {
    make: MakePipe,
    /// The microphone chosen (the settings' `mic`).
    mic: String,
    noise_filter: bool,
    #[cfg(target_os = "ios")]
    echo: horizon_voice::settings::Echo,
    #[cfg(target_os = "ios")]
    playout: Arc<Playout>,
}

/// The microphone, opened.
struct Opened {
    pipe: Arc<CapturePipe>,
    #[cfg(target_os = "ios")]
    output: Option<phone::Output>,
}

/// What the microphone's thread is asked to do.
enum Job {
    Open(Plan),
    /// Let go of the microphone, and of this pipe with it (its last hold is dropped there,
    /// where waiting for its thread to end holds nothing up).
    Close(Option<Arc<CapturePipe>>),
    /// The audio session back to playing only.
    #[cfg(target_os = "ios")]
    Rest,
}

/// The microphone's own thread. Opening and closing a microphone, and on an iPhone
/// switching the audio session between playing and recording, are calls into the system
/// that take a moment (a noticeable one on a phone): made here, the game's frames never wait
/// for them. The device is opened, held and dropped on this thread alone.
struct Worker {
    jobs: std::sync::mpsc::Sender<Job>,
    opened: std::sync::mpsc::Receiver<std::result::Result<Opened, String>>,
}

impl Worker {
    fn start() -> Option<Self> {
        let (jobs, asked) = std::sync::mpsc::channel::<Job>();
        let (answer, opened) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("horizon-voice-device".into())
            .spawn(move || {
                let mut device: Option<Device> = None;
                for job in asked {
                    match job {
                        Job::Open(plan) => {
                            // One microphone at a time: the one before goes first.
                            device = None;
                            let _ = answer.send(match open(plan) {
                                Ok((opened, held)) => {
                                    device = Some(held);
                                    Ok(opened)
                                }
                                Err(e) => Err(e),
                            });
                        }
                        Job::Close(pipe) => {
                            device = None;
                            drop(pipe);
                        }
                        #[cfg(target_os = "ios")]
                        Job::Rest => phone::end_recording(),
                    }
                }
                drop(device);
            })
            .ok()?;
        Some(Self { jobs, opened })
    }
}

/// The iPhone's microphone: the audio session set to record from the one chosen, and the
/// echo of the others' voices kept out when they come from the speaker (the system's echo
/// canceller, or failing that the microphone turned down while they talk).
#[cfg(target_os = "ios")]
fn open(plan: Plan) -> std::result::Result<(Opened, Device), String> {
    use horizon_voice::settings::Echo;
    let choice = match plan.mic.as_str() {
        "builtin" => phone::Choice::BuiltIn,
        "wired" => phone::Choice::Wired,
        "bluetooth" => phone::Choice::Bluetooth,
        _ => phone::Choice::Automatic,
    };
    phone::begin_recording(choice)?;
    let output = phone::route().map(|r| r.output);
    let open_air = output.is_none_or(|o| o.open_air());
    let (system, guard) = match plan.echo {
        Echo::Auto => (open_air, false),
        Echo::System => (true, false),
        Echo::Guard => (false, open_air),
        Echo::Off => (false, false),
    };
    let attempt = |system: bool, guard: bool| -> std::result::Result<(Opened, Device), String> {
        let pipe = (plan.make)(CaptureOptions {
            echo_guard: guard.then(|| plan.playout.clone()),
            // The system's voice processing levels the microphone itself, and filters its
            // noise.
            level_as_is: system,
            noise_filter: plan.noise_filter && !system,
        })?;
        let feed = pipe.clone();
        let capture = phone::start_capture(
            system,
            Box::new(move |samples, channels, rate| feed.feed(samples, channels, rate)),
        )?;
        Ok((Opened { pipe, output }, capture))
    };
    attempt(system, guard).or_else(|e| {
        if !system {
            return Err(e);
        }
        // Without the system's echo canceller: the microphone turned down while the others
        // are heard on the speaker.
        log::warn!("Voice chat: the system's voice processing didn't start ({e})");
        attempt(false, open_air)
    })
}

/// The microphone the game's sound library opens (a desktop, Android).
#[cfg(not(target_os = "ios"))]
fn open(plan: Plan) -> std::result::Result<(Opened, Device), String> {
    let pipe = (plan.make)(CaptureOptions {
        noise_filter: plan.noise_filter,
        ..CaptureOptions::default()
    })?;
    let feed = pipe.clone();
    let capture = horizon_client::audio_capture::open(
        &plan.mic,
        Box::new(move |samples, channels, rate| feed.feed(samples, channels, rate)),
    )?;
    Ok((Opened { pipe }, capture))
}

/// The sound output being watched after the system touched the audio session: when it stops
/// asking for sound and doesn't start again by itself, it is started again.
#[cfg(target_os = "ios")]
struct Watched {
    until: Instant,
    /// The output's count of callbacks when last looked at, and when that was.
    seen: u64,
    at: Instant,
    kicks: u8,
}

/// The session stays set for recording this long after the microphone goes off, so that
/// turning it off and on again doesn't make the sound hiccup twice.
#[cfg(target_os = "ios")]
const SESSION_KEPT_FOR: Duration = Duration::from_secs(15);

/// One player in voice chat's reach, for the screens.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Person {
    pub uuid: u128,
    pub name: String,
    /// Voice chat is on in their game, and their microphone is.
    pub enabled: bool,
    pub mic_live: bool,
    /// How this player hears them: 0 to 2, and not at all.
    pub volume: f32,
    pub muted: bool,
    pub talking: bool,
    pub level: f32,
    pub delay_ms: u32,
}

/// How long to wait before trying the next Simple Voice Chat protocol version (`21 -> 20 -> 18`).
const SVC_PROBE_INTERVAL: Duration = Duration::from_millis(800);

/// Strips the Minecraft TCP port from a `host:port` or `[ipv6]:port` server address.
fn strip_mc_port(addr: &str) -> String {
    let trimmed = addr.trim();
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        return rest[..end].to_owned();
    }
    if trimmed.matches(':').count() == 1
        && let Some((host, _)) = trimmed.split_once(':')
    {
        return host.to_owned();
    }
    trimmed.to_owned()
}

/// Simple Voice Chat (`henkelmax/simple-voice-chat`) session state for external servers.
struct SvcSession {
    probe_idx: usize,
    probe_at: Option<Instant>,
    probed_version: Option<SvcVersion>,
    secret: Option<SecretInfo>,
    udp: Option<SvcUdpClient>,
    server_host: String,
    players: HashMap<u128, SvcPlayerState>,
    groups: HashMap<u128, ClientGroup>,
    joined_group: Option<u128>,
    told_state: Option<(bool, bool)>,
    failure_noted: bool,
    mic_seq: Arc<AtomicI64>,
}

impl Default for SvcSession {
    fn default() -> Self {
        Self {
            probe_idx: 0,
            probe_at: None,
            probed_version: None,
            secret: None,
            udp: None,
            server_host: String::new(),
            players: HashMap::new(),
            groups: HashMap::new(),
            joined_group: None,
            told_state: None,
            failure_noted: false,
            mic_seq: Arc::new(AtomicI64::new(0)),
        }
    }
}

impl SvcSession {
    fn is_active(&self) -> bool {
        self.secret.is_some()
    }

    fn groups_enabled(&self) -> bool {
        self.secret.as_ref().is_some_and(|s| s.groups_enabled)
    }

    fn clear(&mut self) {
        self.probe_idx = 0;
        self.probe_at = None;
        self.probed_version = None;
        self.secret = None;
        self.udp = None;
        self.server_host.clear();
        self.players.clear();
        self.groups.clear();
        self.joined_group = None;
        self.told_state = None;
        self.failure_noted = false;
    }
}

pub(super) struct Voice {
    settings: Settings,
    /// The choices changed and aren't written yet, since when.
    unsaved: Option<Instant>,
    playout: Arc<Playout>,
    /// Voice chat is on: the others are heard.
    enabled: bool,
    /// The microphone is meant to be live.
    mic_wanted: bool,
    mic: Option<Mic>,
    /// The connection voice chat runs on now, and its way out.
    link: Option<Arc<Link>>,
    sender: Option<Sender>,
    /// This game said it has voice chat on that connection; the number the host gave it.
    joined: bool,
    id: Option<u8>,
    roster: Vec<Entry>,
    /// What the host was last told.
    told_state: Option<(bool, bool)>,
    told_unwanted: Option<Vec<u8>>,
    /// Simple Voice Chat session (active on external servers running SVC).
    svc: SvcSession,
    /// The lines already shown this session: that it is there, and who uses it.
    hinted: bool,
    hinted_users: HashSet<u128>,
    /// Every line shown, for the end-to-end checks.
    lines: Vec<String>,
    /// When the connection went with the microphone live.
    mic_lost: Option<Instant>,
    probe: Option<Probe>,
    /// The checks' tone, when one was asked for.
    tone: Option<f32>,
    /// The Friends screen shows voice chat's page; the slider held on it, the first of the
    /// people it shows, and a line about what was just done.
    page: bool,
    /// Whether Voice Chat was opened directly from the Pause menu (so Back returns to Pause).
    return_to_pause: bool,
    held: Option<Held>,
    first: usize,
    note: Option<(String, Instant)>,
    /// Groups tab & form state (used on Simple Voice Chat servers with groups enabled).
    tab: Tab,
    create_mode: bool,
    join_password_for: Option<(u128, String)>,
    name_input: String,
    password_input: String,
    group_type: GroupType,
    focused_field: Option<GroupField>,
    groups_first: usize,
    /// The microphone's own thread, once a microphone was ever asked for, and whether it
    /// is opening one now.
    worker: Option<Worker>,
    opening: bool,
    /// Who is talking, for the HUD: looked up a few times a second, not every frame.
    talkers: Vec<String>,
    talkers_at: Option<Instant>,
    /// Where sound comes out, for the microphones to offer: asked of the system once a
    /// second at most.
    #[cfg(target_os = "ios")]
    route_seen: std::cell::Cell<Option<(Instant, Option<phone::Output>)>>,
    /// The player was asked for the microphone this session (the system asks once ever).
    #[cfg(any(target_os = "android", target_os = "ios"))]
    asked: bool,
    /// The ring switch's line was shown this session.
    told_switch: bool,
    /// The audio session is set for recording; since when the microphone has been off.
    #[cfg(target_os = "ios")]
    recording: bool,
    #[cfg(target_os = "ios")]
    idle: Option<Instant>,
    #[cfg(target_os = "ios")]
    watched: Option<Watched>,
}

fn file() -> PathBuf {
    horizon_client::logging::player_dir().join(FILE)
}

fn testing() -> bool {
    std::env::var_os("HORIZON_VOICE_TEST").is_some_and(|v| v == "1")
}

impl Voice {
    pub(super) fn new() -> Self {
        let settings = std::fs::read_to_string(file())
            .map(|text| Settings::parse(&text))
            .unwrap_or_default();
        let playout = Arc::new(Playout::default());
        playout.set_master(settings.voices);
        Self {
            settings,
            unsaved: None,
            playout,
            enabled: false,
            mic_wanted: false,
            mic: None,
            link: None,
            sender: None,
            joined: false,
            id: None,
            roster: Vec::new(),
            told_state: None,
            told_unwanted: None,
            svc: SvcSession::default(),
            hinted: false,
            hinted_users: HashSet::new(),
            lines: Vec::new(),
            mic_lost: None,
            probe: None,
            tone: None,
            page: false,
            return_to_pause: false,
            held: None,
            first: 0,
            note: None,
            tab: Tab::People,
            create_mode: false,
            join_password_for: None,
            name_input: String::new(),
            password_input: String::new(),
            group_type: GroupType::Normal,
            focused_field: None,
            groups_first: 0,
            worker: None,
            opening: false,
            talkers: Vec::new(),
            talkers_at: None,
            #[cfg(target_os = "ios")]
            route_seen: std::cell::Cell::new(None),
            #[cfg(any(target_os = "android", target_os = "ios"))]
            asked: false,
            told_switch: false,
            #[cfg(target_os = "ios")]
            recording: false,
            #[cfg(target_os = "ios")]
            idle: None,
            #[cfg(target_os = "ios")]
            watched: None,
        }
    }

    /// The Friends screen shows voice chat's page.
    pub(super) fn page_open(&self) -> bool {
        self.page
    }

    /// Voice chat is on (the others are heard).
    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    /// The microphone is live.
    pub(super) fn mic_live(&self) -> bool {
        self.mic.is_some()
    }

    /// The host carries voices and this game is in their reach.
    pub(super) fn connected(&self) -> bool {
        self.id.is_some() || self.svc.udp.as_ref().is_some_and(|u| u.is_connected())
    }

    /// How loud the microphone hears now, and whether speech is going out.
    pub(super) fn mic_level(&self) -> (f32, bool) {
        self.mic
            .as_ref()
            .map_or((0.0, false), |m| (m.pipe.level(), m.pipe.speaking()))
    }

    pub(super) fn voices_volume(&self) -> f32 {
        self.settings.voices
    }

    /// The microphone chosen ("" for the automatic choice).
    pub(super) fn mic_choice(&self) -> &str {
        &self.settings.mic
    }

    /// The others in reach, as this player hears them.
    pub(super) fn people(&self) -> Vec<Person> {
        let heard = self.playout.heard();
        if let Some(secret) = &self.svc.secret {
            let mut list: Vec<Person> = self
                .svc
                .players
                .values()
                .filter(|p| p.uuid != secret.player_uuid && !p.disconnected)
                .map(|p| {
                    let uuid_hex = format!("{:032x}", p.uuid);
                    let slot = self.svc.udp.as_ref().map(|u| u.slot_for(p.uuid));
                    let now = slot.and_then(|s| heard.iter().find(|h| h.id == s));
                    Person {
                        uuid: p.uuid,
                        name: p.name.clone(),
                        enabled: !p.disabled,
                        mic_live: !p.disabled && !p.muted,
                        volume: self.settings.volume_of(&uuid_hex),
                        muted: self.settings.is_muted(&uuid_hex),
                        talking: now.is_some_and(|h| h.talking),
                        level: now.map_or(0.0, |h| h.level),
                        delay_ms: now.map_or(0, |h| h.delay_ms),
                    }
                })
                .collect();
            list.sort_by_key(|a| a.name.to_lowercase());
            return list;
        }
        self.roster
            .iter()
            .filter(|e| Some(e.id) != self.id)
            .map(|e| {
                let uuid = format!("{:032x}", e.uuid);
                let now = heard.iter().find(|h| h.id == e.id);
                Person {
                    uuid: e.uuid,
                    name: e.name.clone(),
                    enabled: e.enabled,
                    mic_live: e.mic_live,
                    volume: self.settings.volume_of(&uuid),
                    muted: self.settings.is_muted(&uuid),
                    talking: now.is_some_and(|h| h.talking),
                    level: now.map_or(0.0, |h| h.level),
                    delay_ms: now.map_or(0, |h| h.delay_ms),
                }
            })
            .collect()
    }

    /// Known Simple Voice Chat groups, sorted deterministically.
    fn svc_group_views(&self) -> Vec<GroupView> {
        let mut counts: HashMap<u128, usize> = HashMap::new();
        for p in self.svc.players.values() {
            if !p.disconnected
                && let Some(gid) = p.group
            {
                *counts.entry(gid).or_insert(0) += 1;
            }
        }
        let mut list: Vec<GroupView> = self
            .svc
            .groups
            .values()
            .filter(|g| !g.hidden)
            .map(|g| GroupView {
                id: g.id,
                name: g.name.clone(),
                has_password: g.has_password,
                kind: g.kind,
                members: counts.get(&g.id).copied().unwrap_or(0),
                joined: self.svc.joined_group == Some(g.id),
            })
            .collect();
        list.sort_by(|a, b| {
            b.joined
                .cmp(&a.joined)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        list
    }

    /// Everyone's settings as the engine plays them, and the speakers the host needn't send.
    fn apply(&mut self) {
        if let Some(secret) = &self.svc.secret {
            if let Some(udp) = &self.svc.udp {
                for p in self.svc.players.values() {
                    if p.uuid == secret.player_uuid {
                        continue;
                    }
                    let uuid = format!("{:032x}", p.uuid);
                    let (volume, muted) = (
                        self.settings.volume_of(&uuid),
                        self.settings.is_muted(&uuid),
                    );
                    let slot = udp.slot_for(p.uuid);
                    self.playout.set_speaker(slot, volume, muted);
                }
            }
            self.playout.set_master(self.settings.voices);
            return;
        }
        let mut unwanted = Vec::new();
        let mut ids = Vec::new();
        for e in &self.roster {
            if Some(e.id) == self.id {
                continue;
            }
            let uuid = format!("{:032x}", e.uuid);
            let (volume, muted) = (
                self.settings.volume_of(&uuid),
                self.settings.is_muted(&uuid),
            );
            self.playout.set_speaker(e.id, volume, muted);
            ids.push(e.id);
            if muted || volume == 0.0 {
                unwanted.push(e.id);
            }
        }
        self.playout.keep(&ids);
        self.playout.set_master(self.settings.voices);
        if self.told_unwanted.as_ref() != Some(&unwanted)
            && self.id.is_some()
            && let Some(sender) = &self.sender
        {
            sender.send(
                Up::Listen {
                    muted: unwanted.clone(),
                }
                .encode(),
            );
            self.told_unwanted = Some(unwanted);
        }
    }

    fn changed(&mut self) {
        self.unsaved.get_or_insert_with(Instant::now);
        self.apply();
    }

    pub(super) fn set_volume(&mut self, uuid: u128, volume: f32) {
        if self.settings.set_volume(&format!("{uuid:032x}"), volume) {
            self.changed();
        }
    }

    pub(super) fn set_muted(&mut self, uuid: u128, muted: bool) {
        if self.settings.set_muted(&format!("{uuid:032x}"), muted) {
            self.changed();
        }
    }

    pub(super) fn set_voices_volume(&mut self, volume: f32) {
        let volume = volume.clamp(0.0, horizon_voice::engine::MAX_VOLUME);
        if self.settings.voices != volume {
            self.settings.voices = volume;
            self.changed();
        }
    }

    pub(super) fn set_mic_choice(&mut self, name: &str) {
        if self.settings.mic != name {
            self.settings.mic = name.to_owned();
            self.unsaved.get_or_insert_with(Instant::now);
            // The microphone in use changes with the choice.
            self.close_mic();
        }
    }

    /// Writes the choices once they have stood a moment (or at once, with `now`).
    fn save(&mut self, now: bool) {
        let due = self
            .unsaved
            .is_some_and(|since| now || since.elapsed() >= Duration::from_secs(1));
        if !due {
            return;
        }
        self.unsaved = None;
        let text = self.settings.to_json();
        let write = move || {
            if let Err(e) = horizon_shared_worlds::scan::write_atomic(&file(), text.as_bytes()) {
                log::warn!("Voice chat: its settings weren't written: {e}");
            }
        };
        // Written to the disk for good (synced): not something a frame waits for, unless
        // the game is closing.
        if now {
            write();
        } else if let Err(e) = std::thread::Builder::new()
            .name("horizon-voice-save".into())
            .spawn(write)
        {
            log::warn!("Voice chat: its settings weren't written: {e}");
        }
    }

    /// Asks the microphone's thread to let go of the microphone, and of `pipe` with it.
    fn close(&mut self, pipe: Option<Arc<CapturePipe>>) {
        match &self.worker {
            Some(worker) => {
                if worker.jobs.send(Job::Close(pipe)).is_err() {
                    self.worker = None;
                }
            }
            // No device was ever opened: only a pipe (the checks' tone's) to drop.
            None => drop(pipe),
        }
    }

    /// The microphone goes off, at once as far as anyone hears (the host stops passing it
    /// on as soon as it is told); the device is let go on its own thread. True when it was
    /// live.
    fn close_mic(&mut self) -> bool {
        let Some(mic) = self.mic.take() else {
            return false;
        };
        let pipe = mic.pipe.clone();
        // The tone's thread, and this hold of the pipe.
        drop(mic);
        self.close(Some(pipe));
        true
    }

    /// The microphone is meant to be on (it is, or is being opened): what the button and
    /// the page show, the moment it is asked for.
    pub(super) fn mic_on(&self) -> bool {
        self.mic_wanted
    }

    /// The sound output hears the voices, or doesn't.
    fn listen(&mut self, on: bool) {
        if let Some(link) = &self.link {
            link.attach(on.then(|| self.playout.clone()));
        }
        if testing() {
            // No sound output in the checks: their own ears, at an output's pace.
            self.probe = on.then(|| Probe::start(self.playout.clone()));
        } else {
            let tap: Option<Arc<dyn horizon_client::audio::StreamSource>> = if on {
                Some(Arc::new(Tap(self.playout.clone())))
            } else {
                None
            };
            horizon_client::audio::set_stream_source(tap);
        }
        if !on {
            self.playout.clear();
        }
    }

    /// Forgets the connection (it ended, or the session did).
    fn disconnected(&mut self) {
        if self.close_mic() {
            self.mic_lost = Some(Instant::now());
        }
        if let Some(link) = self.link.take() {
            link.attach(None);
        }
        self.sender = None;
        self.joined = false;
        self.id = None;
        self.roster.clear();
        self.told_state = None;
        self.told_unwanted = None;
        self.svc.clear();
        self.playout.clear();
    }

    /// The session ended: everything is as before a world is joined.
    fn reset(&mut self) {
        self.disconnected();
        if self.enabled {
            self.listen(false);
        }
        self.enabled = false;
        self.mic_wanted = false;
        self.mic_lost = None;
        self.hinted = false;
        self.hinted_users.clear();
        self.tone = None;
        self.page = false;
        self.return_to_pause = false;
        self.held = None;
        self.first = 0;
        self.note = None;
        self.tab = Tab::People;
        self.create_mode = false;
        self.join_password_for = None;
        self.name_input.clear();
        self.password_input.clear();
        self.group_type = GroupType::Normal;
        self.focused_field = None;
        self.groups_first = 0;
        #[cfg(target_os = "ios")]
        {
            self.asked = false;
        }
        self.told_switch = false;
        self.save(true);
    }
}

impl Game<'_> {
    /// A line of voice chat's in chat: coloured, so it isn't missed.
    fn voice_line(&mut self, text: String) {
        self.voice.lines.push(text.clone());
        if self.voice.lines.len() > 50 {
            self.voice.lines.remove(0);
        }
        self.system_message(Text::literal(text).with_color(HINT).bold());
    }

    /// Voice chat can be turned on here: a shared world whose host carries voices, or an
    /// external server running Simple Voice Chat.
    pub(super) fn voice_offered(&self) -> bool {
        (self.shared_active()
            && self
                .remote
                .as_ref()
                .is_some_and(|r| r.session.voice.offered()))
            || self.voice.svc.is_active()
    }

    /// Turns voice chat on (the others are heard; the microphone stays off) or off.
    pub(super) fn voice_set_enabled(&mut self, on: bool) {
        if self.voice.enabled == on || (on && !self.voice_offered()) {
            return;
        }
        self.voice.enabled = on;
        self.voice.listen(on);
        if on {
            if let Some(info) = self.voice.svc.secret.clone()
                && self
                    .voice
                    .svc
                    .udp
                    .as_ref()
                    .is_none_or(|u| u.failure().is_some())
            {
                self.voice_connect_svc_udp(info);
            }
            self.voice.apply();
            self.voice_line(tr(
                "on",
                "Voice chat is on: you hear the others. Your microphone is off until you turn it on.",
            ));
        } else {
            self.voice.mic_wanted = false;
            self.voice.close_mic();
            self.voice.mic_lost = None;
            self.voice_line(tr("off", "Voice chat is off."));
        }
    }

    /// Turns the microphone on or off (only with voice chat on).
    pub(super) fn voice_set_mic(&mut self, live: bool) {
        if !self.voice.enabled {
            return;
        }
        self.voice.mic_wanted = live;
        self.voice.mic_lost = None;
        if !live {
            self.voice.close_mic();
        }
    }

    pub(super) fn voice_toggle_mic(&mut self) {
        let live = !self.voice.mic_wanted;
        self.voice_set_mic(live);
    }

    /// Whether the microphone may be started: yes, no, or not known yet (the player is
    /// being asked, the first time ever).
    fn voice_mic_allowed(&mut self) -> Option<bool> {
        #[cfg(target_os = "ios")]
        if self.voice.tone.is_none() {
            return match phone::mic_permission() {
                phone::Permission::Granted => Some(true),
                phone::Permission::Denied => Some(false),
                phone::Permission::Unknown => {
                    if !std::mem::replace(&mut self.voice.asked, true) {
                        phone::request_mic_permission();
                    }
                    None
                }
            };
        }
        #[cfg(target_os = "android")]
        if self.voice.tone.is_none() {
            return match crate::android::mic_permission() {
                1 => Some(true),
                -1 => Some(false),
                _ => {
                    if !std::mem::replace(&mut self.voice.asked, true) {
                        crate::android::request_mic_permission();
                    }
                    None
                }
            };
        }
        Some(true)
    }

    /// Starts the microphone: its pipe to the connection, and the device (or the checks'
    /// tone) that feeds it.
    fn voice_start_mic(&mut self) {
        let make: MakePipe = if let Some(udp) = &self.voice.svc.udp {
            let mic_sender = udp.mic_sender();
            let seq = self.voice.svc.mic_seq.clone();
            Box::new(move |options: CaptureOptions| {
                let mic_sender = mic_sender.clone();
                let seq = seq.clone();
                CapturePipe::start_raw(
                    Box::new(move |_frame_seq, flags, opus| {
                        if !opus.is_empty() {
                            let s = seq.fetch_add(1, Ordering::Relaxed);
                            mic_sender.send_mic(s, opus, false);
                        }
                        if (flags & END) != 0 {
                            let s = seq.fetch_add(1, Ordering::Relaxed);
                            mic_sender.send_mic(s, &[], false);
                        }
                    }),
                    options,
                )
                .map(Arc::new)
                .map_err(|e| e.to_string())
            })
        } else {
            let Some(sender) = self.voice.sender.clone() else {
                return;
            };
            Box::new(move |options: CaptureOptions| {
                let sender = sender.clone();
                CapturePipe::start(Box::new(move |message| sender.send(message)), options)
                    .map(Arc::new)
                    .map_err(|e| e.to_string())
            })
        };
        // The checks' tone is no device: nothing to wait for.
        if let Some(hz) = self.voice.tone {
            match make(CaptureOptions::default()) {
                Ok(pipe) => {
                    self.voice.mic = Some(Mic {
                        _tone: Some(Tone::start(pipe.clone(), hz)),
                        pipe,
                        #[cfg(target_os = "ios")]
                        output: None,
                    });
                }
                Err(e) => self.voice_mic_failed(&e),
            }
            return;
        }
        // The device is opened on the microphone's own thread; the game goes on meanwhile.
        if self.voice.worker.is_none() {
            self.voice.worker = Worker::start();
        }
        let plan = Plan {
            make,
            mic: self.voice.settings.mic.clone(),
            noise_filter: self.voice.settings.noise_filter,
            #[cfg(target_os = "ios")]
            echo: self.voice.settings.echo,
            #[cfg(target_os = "ios")]
            playout: self.voice.playout.clone(),
        };
        let sent = self
            .voice
            .worker
            .as_ref()
            .is_some_and(|w| w.jobs.send(Job::Open(plan)).is_ok());
        if !sent {
            self.voice.worker = None;
            self.voice_mic_failed("the game couldn't start a thread for it");
            return;
        }
        self.voice.opening = true;
        #[cfg(target_os = "ios")]
        {
            self.voice.recording = true;
            self.voice.idle = None;
        }
    }

    fn voice_mic_failed(&mut self, why: &str) {
        self.voice.mic_wanted = false;
        self.voice_line(
            tr("mic_failed", "The microphone couldn't be started: %s").replacen("%s", why, 1),
        );
    }

    /// What the microphone's thread has to say: the microphone is open (and is this game's,
    /// if it is still wanted), or couldn't be opened.
    fn voice_mic_opened(&mut self) {
        let Some(answer) = self
            .voice
            .worker
            .as_ref()
            .and_then(|w| w.opened.try_recv().ok())
        else {
            return;
        };
        self.voice.opening = false;
        #[cfg(target_os = "ios")]
        self.voice_watch_output();
        match answer {
            Ok(opened) => {
                // Turned off again while it was being opened, or the connection went.
                if !(self.voice.enabled && self.voice.mic_wanted && self.voice.connected()) {
                    self.voice.close(Some(opened.pipe));
                    return;
                }
                self.voice.mic = Some(Mic {
                    pipe: opened.pipe,
                    _tone: None,
                    #[cfg(target_os = "ios")]
                    output: opened.output,
                });
                // An iPhone with its microphone live plays through the ring switch.
                if cfg!(target_os = "ios") && !std::mem::replace(&mut self.voice.told_switch, true)
                {
                    self.voice_line(tr(
                        "ring_switch",
                        "While your microphone is on, the ring/silent switch doesn't silence the game: iOS treats a live microphone like a call.",
                    ));
                }
            }
            Err(e) => self.voice_mic_failed(&e),
        }
    }

    /// The app left the screen: the microphone goes off, and stays off when it comes back.
    pub(super) fn voice_backgrounded(&mut self) {
        if self.voice.mic_wanted || self.voice.mic.is_some() {
            self.voice.mic_wanted = false;
            self.voice.close_mic();
            self.voice.mic_lost = None;
        }
    }

    /// The sound output is watched for a while: the system may have stopped it.
    #[cfg(target_os = "ios")]
    fn voice_watch_output(&mut self) {
        let now = Instant::now();
        self.voice.watched = Some(Watched {
            until: now + Duration::from_secs(30),
            seen: horizon_client::audio::output_callbacks(),
            at: now,
            kicks: 0,
        });
    }

    /// What the system did to the audio session, and what follows from it: an interruption
    /// (a call, Siri) turns the microphone off; headphones gone turn voice chat off, so the
    /// others aren't suddenly heard out loud; another output starts the microphone again
    /// for it; and a sound output the system stopped is started again.
    #[cfg(target_os = "ios")]
    fn voice_platform(&mut self) {
        // Nothing to hear about until voice chat was ever used.
        if !(self.voice.enabled || self.voice.recording || self.voice.watched.is_some()) {
            return;
        }
        for event in phone::events() {
            match event {
                phone::Event::Interrupted => {
                    if self.voice.mic_wanted || self.voice.mic.is_some() {
                        self.voice.mic_wanted = false;
                        self.voice.close_mic();
                        self.voice_line(tr(
                            "mic_interrupted",
                            "Your microphone was turned off: a call or another app took the sound.",
                        ));
                    }
                    self.voice_watch_output();
                }
                phone::Event::Resumed | phone::Event::Reset => {
                    // Started again from scratch when it is still wanted.
                    self.voice.close_mic();
                    self.voice_watch_output();
                }
                phone::Event::OutputLost => {
                    if self.voice.enabled {
                        self.voice_set_enabled(false);
                        self.voice_line(tr(
                            "headphones_gone",
                            "Voice chat was turned off because your headphones disconnected.",
                        ));
                    }
                    self.voice_watch_output();
                }
                phone::Event::RouteChanged => {
                    let now = phone::route().map(|r| r.output);
                    if self.voice.mic.as_ref().is_some_and(|m| m.output != now) {
                        self.voice.close_mic();
                    }
                    self.voice_watch_output();
                }
            }
        }
        // The microphone has been off a while: the session goes back to playing only.
        if self.voice.mic.is_some() || self.voice.opening || !self.voice.recording {
            self.voice.idle = None;
        } else {
            let since = *self.voice.idle.get_or_insert_with(Instant::now);
            if since.elapsed() >= SESSION_KEPT_FOR || !self.voice.enabled {
                // On the microphone's thread, as every change of the session is.
                if let Some(worker) = &self.voice.worker {
                    let _ = worker.jobs.send(Job::Rest);
                }
                self.voice.recording = false;
                self.voice.idle = None;
                self.voice_watch_output();
            }
        }
        // A sound output that stopped asking for sound and didn't start again by itself.
        let callbacks = horizon_client::audio::output_callbacks();
        let mut kick = false;
        let mut done = false;
        if let Some(watched) = &mut self.voice.watched {
            let now = Instant::now();
            if callbacks != watched.seen {
                (watched.seen, watched.at) = (callbacks, now);
            } else if now.duration_since(watched.at) >= Duration::from_millis(1500) {
                watched.at = now;
                watched.kicks += 1;
                kick = true;
            }
            done = now >= watched.until || watched.kicks >= 5;
        }
        if done {
            self.voice.watched = None;
        }
        if kick {
            log::warn!("Voice chat: the sound output stood still: starting it again");
            self.audio.suspend_output();
            self.audio.resume_output();
        }
    }

    /// Opens the microphone when wanted and permitted (shared by Horizon voice & Simple Voice Chat).
    fn voice_tick_mic(&mut self) {
        self.voice_mic_opened();
        if self.voice.enabled
            && self.voice.mic_wanted
            && self.voice.mic.is_none()
            && !self.voice.opening
        {
            match self.voice_mic_allowed() {
                Some(true) => {
                    self.voice.mic_lost = None;
                    self.voice_start_mic();
                }
                Some(false) => {
                    self.voice.mic_wanted = false;
                    self.voice_line(tr(
                        "mic_denied",
                        "The microphone isn't allowed. Allow it in the Settings app (Horizon PE > Microphone), then turn it on again here.",
                    ));
                }
                // The player is being asked.
                None => {}
            }
        }
    }

    /// World eye position of another player currently tracked in the level, for 3D proximity audio.
    fn voice_remote_player_pos(&self, uuid: u128) -> Option<[f64; 3]> {
        let entity_id = self.remote.as_ref()?.player_entity(uuid)?;
        let mob = self.mobs.iter().find(|m| m.id == entity_id)?;
        Some([mob.position.x, mob.position.y + 1.62, mob.position.z])
    }

    /// Starts (or restarts) the Simple Voice Chat encrypted UDP transport from `info`.
    fn voice_connect_svc_udp(&mut self, info: SecretInfo) {
        self.voice.svc.failure_noted = false;
        match SvcUdpClient::connect(
            info,
            &self.voice.svc.server_host,
            self.voice.playout.clone(),
        ) {
            Ok(client) => {
                self.voice.svc.udp = Some(client);
            }
            Err(e) => {
                log::warn!("Simple Voice Chat: failed to start UDP transport: {e}");
                self.voice.svc.udp = None;
                self.voice.svc.failure_noted = true;
                self.voice.note = Some((
                    tr("svc_udp_failed", "Voice chat UDP couldn't connect: %s")
                        .replacen("%s", &e, 1),
                    Instant::now(),
                ));
            }
        }
    }

    /// Applies 3D proximity attenuation and stereo panning for every active SVC speaker slot.
    fn voice_sync_spatial(&mut self) {
        let Some(udp) = &self.voice.svc.udp else {
            return;
        };
        let Some(player) = &self.player else {
            return;
        };
        let listener = [
            player.position.x,
            player.position.y + 1.62,
            player.position.z,
        ];
        let yaw = self.yaw;
        let default_dist = self
            .voice
            .svc
            .secret
            .as_ref()
            .map_or(48.0, |s| s.voice_distance as f32);
        for (slot, source) in udp.spatial_snapshot() {
            match source {
                SpatialSource::Player { uuid, distance, .. } => {
                    let max_dist = if distance > 0.0 {
                        distance
                    } else {
                        default_dist
                    };
                    if let Some(pos) = self.voice_remote_player_pos(uuid) {
                        let (atten, pan) = spatial_params(listener, yaw, pos, max_dist);
                        self.voice.playout.set_spatial(slot, atten, pan);
                    } else {
                        self.voice.playout.set_spatial(slot, 0.5, 0.0);
                    }
                }
                SpatialSource::Group { .. } => {
                    self.voice.playout.set_spatial(slot, 1.0, 0.0);
                }
                SpatialSource::Location { pos, distance } => {
                    let max_dist = if distance > 0.0 {
                        distance
                    } else {
                        default_dist
                    };
                    let (atten, pan) = spatial_params(listener, yaw, pos, max_dist);
                    self.voice.playout.set_spatial(slot, atten, pan);
                }
            }
        }
    }

    /// Handles one incoming `voicechat:*` TCP plugin message from a Simple Voice Chat server.
    fn voice_handle_svc_packet(&mut self, channel: &str, data: &[u8]) {
        if channel == CH_SECRET {
            let mut decoded = None;
            if let Some(v) = self.voice.svc.probed_version {
                decoded = decode_secret(v, data);
            }
            if decoded.is_none() {
                for v in SvcVersion::PROBE_ORDER {
                    if let Some(info) = decode_secret(v, data) {
                        decoded = Some(info);
                        break;
                    }
                }
            }
            let Some(info) = decoded else {
                log::warn!(
                    "Simple Voice Chat: ignoring unreadable voicechat:secret ({} bytes)",
                    data.len()
                );
                return;
            };
            let first_connect = self.voice.svc.secret.is_none();
            self.voice.svc.secret = Some(info.clone());
            self.voice.svc.told_state = None;
            self.voice_connect_svc_udp(info);
            // Auto-connect listening on join, with microphone muted by default.
            if first_connect && !self.voice.enabled {
                self.voice.enabled = true;
                self.voice.mic_wanted = false;
                self.voice.listen(true);
                self.voice.apply();
                self.voice_line(tr(
                    "svc_connected",
                    "Simple Voice Chat connected (mic muted). Open Game Menu > Voice Chat or tap the mic button to talk.",
                ));
            } else {
                self.voice.apply();
            }
            return;
        }
        let version = self
            .voice
            .svc
            .secret
            .as_ref()
            .map(|s| s.version)
            .or(self.voice.svc.probed_version)
            .unwrap_or(SvcVersion::V21);
        let Some(packet) = decode_tcp(version, channel, data) else {
            return;
        };
        match packet {
            TcpIn::Secret(_) => {}
            TcpIn::PlayerState(state) => {
                if state.disconnected {
                    self.voice.svc.players.remove(&state.uuid);
                } else {
                    self.voice.svc.players.insert(state.uuid, state);
                }
                self.voice.apply();
            }
            TcpIn::PlayerStates(states) => {
                self.voice.svc.players.clear();
                for state in states {
                    if !state.disconnected {
                        self.voice.svc.players.insert(state.uuid, state);
                    }
                }
                self.voice.apply();
            }
            TcpIn::RemovePlayerState(uuid) => {
                self.voice.svc.players.remove(&uuid);
                self.voice.apply();
            }
            TcpIn::AddGroup(group) => {
                self.voice.svc.groups.insert(group.id, group);
            }
            TcpIn::RemoveGroup(id) => {
                self.voice.svc.groups.remove(&id);
                if self.voice.svc.joined_group == Some(id) {
                    self.voice.svc.joined_group = None;
                }
            }
            TcpIn::JoinedGroup {
                group,
                wrong_password,
            } => {
                if wrong_password {
                    self.voice.note =
                        Some((tr("wrong_password", "Wrong password."), Instant::now()));
                    return;
                }
                self.voice.svc.joined_group = group;
                self.voice.create_mode = false;
                self.voice.join_password_for = None;
                self.voice.name_input.clear();
                self.voice.password_input.clear();
                self.voice.focused_field = None;
                if let Some(id) = group
                    && let Some(g) = self.voice.svc.groups.get(&id)
                {
                    let name = g.name.clone();
                    self.voice.note = Some((
                        tr("joined_group", "Joined group: %s").replacen("%s", &name, 1),
                        Instant::now(),
                    ));
                }
            }
        }
    }

    /// Per-frame Simple Voice Chat discovery, packet handling, spatialization, and state sync.
    fn tick_svc_voice(&mut self, link: &Arc<Link>) {
        // Probe SVC versions (21 -> 20 -> 18) until voicechat:secret arrives.
        if self.voice.svc.secret.is_none()
            && self.voice.svc.probe_idx < SvcVersion::PROBE_ORDER.len()
            && self
                .voice
                .svc
                .probe_at
                .is_none_or(|t| t.elapsed() >= SVC_PROBE_INTERVAL)
            && let Some(sender) = &self.voice.sender
        {
            let version = SvcVersion::PROBE_ORDER[self.voice.svc.probe_idx];
            sender.send_custom(CH_REQUEST_SECRET, encode_request_secret(version));
            self.voice.svc.probed_version = Some(version);
            self.voice.svc.probe_at = Some(Instant::now());
            self.voice.svc.probe_idx += 1;
        }

        for (channel, data) in link.take_svc() {
            self.voice_handle_svc_packet(&channel, &data);
        }

        let Some(version) = self.voice.svc.secret.as_ref().map(|s| s.version) else {
            self.voice.save(false);
            return;
        };

        if let Some(err) = self.voice.svc.udp.as_ref().and_then(|u| u.failure())
            && !self.voice.svc.failure_noted
        {
            self.voice.svc.failure_noted = true;
            self.voice.close_mic();
            self.voice.mic_wanted = false;
            self.voice.note = Some((
                tr("svc_udp_lost", "Voice chat connection lost: %s").replacen("%s", &err, 1),
                Instant::now(),
            ));
        }

        if self.voice.enabled {
            self.voice_sync_spatial();
        }
        if self.voice.connected() {
            self.voice_tick_mic();
        }

        let state = (!self.voice.mic_on(), !self.voice.enabled);
        if self.voice.svc.told_state != Some(state)
            && let Some(sender) = &self.voice.sender
        {
            let (ch, payload) = encode_update_state(version, state.0, state.1);
            sender.send_custom(ch, payload);
            self.voice.svc.told_state = Some(state);
        }
        self.voice.save(false);
    }

    /// Voice chat's work for this frame.
    pub(super) fn tick_voice(&mut self) {
        #[cfg(target_os = "ios")]
        self.voice_platform();
        self.voice_talkers();
        let shared = self.shared_active();
        let external = !shared && self.external_server();
        if !shared && !external {
            if self.voice.enabled || self.voice.link.is_some() || self.voice.svc.is_active() {
                self.voice.reset();
            }
            self.voice.save(false);
            return;
        }
        // The connection of the moment: another one after a host change or server switch.
        let link = self
            .remote
            .as_ref()
            .filter(|r| r.joined)
            .map(|r| r.session.voice.clone());
        let same = match (&self.voice.link, &link) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.voice.disconnected();
            if let (Some(link), Some(remote)) = (link, self.remote.as_ref()) {
                self.voice.sender = Some(remote.session.voice_sender());
                if external {
                    self.voice.svc.server_host = strip_mc_port(&remote.session.address);
                } else if self.voice.enabled {
                    link.attach(Some(self.voice.playout.clone()));
                }
                self.voice.link = Some(link);
            }
        }
        let Some(link) = self.voice.link.clone() else {
            // Between two hosts: a microphone that was live waits a while for the next.
            if shared
                && self
                    .voice
                    .mic_lost
                    .is_some_and(|at| at.elapsed() > MIC_KEPT_FOR)
                && self.voice.mic_wanted
            {
                self.voice.mic_wanted = false;
                self.voice.mic_lost = None;
                self.voice_line(tr(
                    "mic_muted_away",
                    "Your microphone was turned off while the world changed hosts.",
                ));
            }
            return;
        };
        if external {
            self.tick_svc_voice(&link);
            return;
        }
        // The host carries voices: this game says it does too, once.
        if link.offered()
            && !self.voice.joined
            && let Some(sender) = &self.voice.sender
        {
            sender.send(Up::Join { version: VERSION }.encode());
            self.voice.joined = true;
        }
        for told in link.take() {
            match told {
                Told::Welcome { id } => {
                    self.voice.id = Some(id);
                    self.voice.told_state = None;
                    self.voice.told_unwanted = None;
                }
                Told::Roster(entries) => {
                    self.voice.roster = entries;
                    self.voice.apply();
                    self.voice_hints();
                }
            }
        }
        if self.voice.id.is_none() {
            return;
        }
        // The microphone, as it is meant to be.
        self.voice_tick_mic();
        let state = (self.voice.enabled, self.voice.mic.is_some());
        if self.voice.told_state != Some(state)
            && let Some(sender) = &self.voice.sender
        {
            sender.send(
                Up::State {
                    enabled: state.0,
                    mic_live: state.1,
                }
                .encode(),
            );
            self.voice.told_state = Some(state);
        }
        self.voice.save(false);
    }

    /// Who is talking, looked up for the HUD a few times a second.
    fn voice_talkers(&mut self) {
        if !self.voice.enabled {
            self.voice.talkers.clear();
            return;
        }
        if self
            .voice
            .talkers_at
            .is_some_and(|at| at.elapsed() < Duration::from_millis(100))
        {
            return;
        }
        self.voice.talkers_at = Some(Instant::now());
        self.voice.talkers = self
            .voice
            .people()
            .into_iter()
            .filter(|p| p.talking && !p.muted)
            .map(|p| p.name)
            .collect();
    }

    /// The lines that say voice chat is there, to a player who has it off: once that it is,
    /// and once for each of the others who uses it.
    fn voice_hints(&mut self) {
        if self.voice.enabled {
            return;
        }
        let me = self.voice.id;
        let others: Vec<Entry> = self
            .voice
            .roster
            .iter()
            .filter(|e| Some(e.id) != me)
            .cloned()
            .collect();
        let mut said_how = false;
        for other in others.iter().filter(|e| e.enabled) {
            if self.voice.hinted_users.insert(other.uuid) {
                self.voice_line(
                    tr(
                        "in_use",
                        "%s is using voice chat. Turn it on in Game Menu > Friends > Voice Chat to hear them.",
                    )
                    .replacen("%s", &other.name, 1),
                );
                said_how = true;
            }
        }
        if !others.is_empty() && !self.voice.hinted {
            self.voice.hinted = true;
            if !said_how {
                self.voice_line(tr(
                    "available",
                    "Voice chat is available in this world. Turn it on in Game Menu > Friends > Voice Chat.",
                ));
            }
        }
    }

    /// The microphones to choose from besides the automatic choice: what is kept for each
    /// (the settings' `mic`) and what the player reads.
    fn voice_mics(&self) -> Vec<(String, String)> {
        #[cfg(target_os = "ios")]
        {
            // Asked of the system once a second at most: the page draws every frame.
            let output = match self.voice.route_seen.get() {
                Some((at, output)) if at.elapsed() < Duration::from_secs(1) => output,
                _ => {
                    let output = phone::route().map(|r| r.output);
                    self.voice.route_seen.set(Some((Instant::now(), output)));
                    output
                }
            };
            let mut mics = vec![("builtin".to_owned(), tr("mic_builtin", "This Device"))];
            if output == Some(phone::Output::Headphones) {
                mics.push(("wired".to_owned(), tr("mic_wired", "Wired Headset")));
            }
            // A Bluetooth headset's own microphone makes everything heard a call's quality.
            if output == Some(phone::Output::Bluetooth) || self.voice.mic_choice() == "bluetooth" {
                mics.push((
                    "bluetooth".to_owned(),
                    tr("mic_bluetooth", "Bluetooth (worse)"),
                ));
            }
            mics
        }
        #[cfg(not(target_os = "ios"))]
        horizon_client::audio_capture::input_devices()
            .into_iter()
            .map(|name| {
                // As much of a long name as the button holds.
                let label = if name.chars().count() > 18 {
                    name.chars().take(17).chain(['.']).collect()
                } else {
                    name.clone()
                };
                (name, label)
            })
            .collect()
    }

    fn voice_view(&self) -> voice_screen::View {
        let (mic_level, speaking) = self.voice.mic_level();
        let chosen = self.voice.mic_choice();
        let groups_enabled = self.voice.svc.groups_enabled();
        voice_screen::View {
            offered: self.voice_offered(),
            enabled: self.voice.enabled(),
            mic_live: self.voice.mic_on(),
            mic_level,
            speaking,
            mic_name: self
                .voice_mics()
                .into_iter()
                .find(|(kept, _)| kept == chosen)
                .map_or_else(|| tr("mic_automatic", "Automatic"), |(_, label)| label),
            mic_choice: !self.voice_mics().is_empty(),
            voices: self.voice.voices_volume(),
            noise_filter: self.voice.settings.noise_filter,
            people: self
                .voice
                .people()
                .into_iter()
                .map(|p| voice_screen::Person {
                    uuid: p.uuid,
                    name: p.name,
                    enabled: p.enabled,
                    mic_live: p.mic_live,
                    volume: p.volume,
                    muted: p.muted,
                    talking: p.talking,
                })
                .collect(),
            first: self.voice.first,
            note: self
                .voice
                .note
                .as_ref()
                .filter(|(_, at)| at.elapsed() < Duration::from_secs(6))
                .map(|(note, _)| note.clone())
                .or_else(|| self.voice_delay_note()),
            groups_enabled,
            tab: if groups_enabled {
                self.voice.tab
            } else {
                Tab::People
            },
            groups: if groups_enabled {
                self.voice.svc_group_views()
            } else {
                Vec::new()
            },
            joined_group: self.voice.svc.joined_group,
            create_mode: self.voice.create_mode,
            join_password_for: self.voice.join_password_for.clone(),
            name_input: self.voice.name_input.clone(),
            password_input: self.voice.password_input.clone(),
            group_type: self.voice.group_type,
            focused_field: self.voice.focused_field,
            groups_first: self.voice.groups_first,
        }
    }

    /// A line for the page when the others' voices play well behind their arrival: the
    /// connection brings them in bursts (through the public tunnel, say), and the buffer that
    /// rides the bursts out is that long.
    fn voice_delay_note(&self) -> Option<String> {
        let behind = self
            .voice
            .people()
            .iter()
            .filter(|p| p.talking)
            .map(|p| p.delay_ms)
            .max()?;
        (behind >= 160).then(|| {
            tr(
                "behind",
                "The connection is uneven: voices play about %d ms behind.",
            )
            .replacen("%d", &(behind / 20 * 20).to_string(), 1)
        })
    }

    /// Opens voice chat's page of the Friends screen.
    pub(super) fn voice_page_show(&mut self) {
        self.voice.page = true;
        self.voice.return_to_pause = false;
        self.voice.held = None;
        self.voice.first = 0;
    }

    /// Opens voice chat's page directly from the Pause menu (so Back/Escape returns to Pause).
    pub(super) fn voice_page_show_from_pause(&mut self) {
        self.voice.page = true;
        self.voice.return_to_pause = true;
        self.voice.held = None;
        self.voice.first = 0;
        self.voice.tab = Tab::People;
        self.voice.create_mode = false;
        self.voice.join_password_for = None;
        self.voice.focused_field = None;
    }

    /// Escape, or Back: closes an open group form first, then closes voice chat's page.
    /// Returns false when the page wasn't open.
    pub(super) fn voice_page_close(&mut self) -> bool {
        self.voice.held = None;
        if self.voice.page && (self.voice.create_mode || self.voice.join_password_for.is_some()) {
            self.voice.create_mode = false;
            self.voice.join_password_for = None;
            self.voice.focused_field = None;
            self.voice.password_input.clear();
            return true;
        }
        let was_open = std::mem::replace(&mut self.voice.page, false);
        self.voice.focused_field = None;
        if was_open && std::mem::take(&mut self.voice.return_to_pause) {
            self.screen = Screen::Paused;
            return true;
        }
        was_open
    }

    /// Whether a text field on the Voice Chat Groups screen currently has keyboard focus.
    pub(super) fn voice_typing(&self) -> bool {
        self.screen == Screen::Friends
            && self.voice.page
            && self.voice.svc.groups_enabled()
            && self.voice.tab == Tab::Groups
            && (self.voice.create_mode || self.voice.join_password_for.is_some())
            && self.voice.focused_field.is_some()
    }

    /// Appends typed text into the currently focused Voice Chat group field.
    pub(super) fn voice_type_text(&mut self, text: &str) {
        let Some(field) = self.voice.focused_field else {
            return;
        };
        let (target, limit) = match field {
            GroupField::Name => (&mut self.voice.name_input, MAX_GROUP_NAME),
            GroupField::Password => (&mut self.voice.password_input, MAX_GROUP_PASSWORD),
        };
        for ch in text.chars().filter(|c| !c.is_control()) {
            if target.chars().count() < limit {
                target.push(ch);
            }
        }
    }

    /// Handles a key press while a Voice Chat group text field has focus.
    pub(super) fn voice_key_input(&mut self, key: KeyCode, text: Option<&str>) {
        if !self.voice_typing() {
            return;
        }
        match key {
            KeyCode::Backspace => match self.voice.focused_field {
                Some(GroupField::Name) => {
                    self.voice.name_input.pop();
                }
                Some(GroupField::Password) => {
                    self.voice.password_input.pop();
                }
                None => {}
            },
            KeyCode::Tab if self.voice.create_mode => {
                self.voice.focused_field = match self.voice.focused_field {
                    Some(GroupField::Name) => Some(GroupField::Password),
                    _ => Some(GroupField::Name),
                };
            }
            KeyCode::Enter | KeyCode::NumpadEnter => {
                if self.voice.create_mode && !self.voice.name_input.trim().is_empty() {
                    self.voice_act(Action::SubmitCreateGroup);
                } else if self.voice.join_password_for.is_some()
                    && !self.voice.password_input.is_empty()
                {
                    self.voice_act(Action::SubmitJoinPassword);
                }
            }
            _ => {
                if let Some(t) = text {
                    self.voice_type_text(t);
                }
            }
        }
    }

    pub(super) fn draw_voice_page(&mut self, batch: &mut HudBatch) {
        let view = self.voice_view();
        horizon_client::widgets::begin_screen(|| {
            horizon_client::widgets::ScreenInfo::new("voice_chat", voice_screen::TITLE)
        });
        voice_screen::draw(
            batch,
            &self.gui.atlas,
            self.cursor,
            &view,
            self.voice.held.as_ref(),
        );
    }

    /// A press on voice chat's page.
    pub(super) fn voice_page_click(&mut self) {
        let view = self.voice_view();
        let Some((action, held)) =
            voice_screen::press(&self.gui.atlas, self.gui_size(), self.cursor, &view)
        else {
            self.voice.focused_field = None;
            return;
        };
        if !matches!(action, Action::FocusField(_)) {
            self.voice.focused_field = None;
        }
        if held.is_none() {
            self.audio.play_button_click();
        }
        self.voice.held = held;
        self.voice_act(action);
    }

    /// The cursor moved with a slider of the page held.
    pub(super) fn voice_page_drag(&mut self) {
        if let Some(held) = self.voice.held {
            let action = held.action(self.cursor.0);
            self.voice_act(action);
        }
    }

    /// The button was let go: a held slider lets go, with the click a slider gives then.
    pub(super) fn voice_page_release(&mut self) {
        if self.voice.held.take().is_some() {
            self.audio.play_button_click();
        }
    }

    /// Whether a finger landing at `cursor` (GUI pixels) is on a slider of the page, which it
    /// drags like a held mouse instead of scrolling.
    pub(super) fn voice_slider_under(&self, cursor: (f32, f32)) -> bool {
        self.voice.page
            && voice_screen::press(&self.gui.atlas, self.gui_size(), cursor, &self.voice_view())
                .is_some_and(|(_, held)| held.is_some())
    }

    fn voice_act(&mut self, action: Action) {
        match action {
            Action::Enable(on) => self.voice_set_enabled(on),
            Action::Mic(live) => self.voice_set_mic(live),
            Action::NextMic => {
                // Automatic, then each of the device's, and round again.
                let mut choices = vec![String::new()];
                choices.extend(self.voice_mics().into_iter().map(|(kept, _)| kept));
                let at = choices
                    .iter()
                    .position(|c| c == self.voice.mic_choice())
                    .unwrap_or(0);
                let next = choices[(at + 1) % choices.len()].clone();
                self.voice.set_mic_choice(&next);
                if next == "bluetooth" {
                    self.voice.note = Some((
                        tr(
                            "bluetooth_note",
                            "A Bluetooth headset's own microphone lowers the quality of everything you hear, as on a call.",
                        ),
                        Instant::now(),
                    ));
                }
            }
            Action::NoiseFilter(on) => {
                self.voice.settings.noise_filter = on;
                self.voice.unsaved.get_or_insert_with(Instant::now);
                // The microphone starts again with it (or without).
                self.voice.close_mic();
                if on {
                    self.voice.note = Some((
                        tr(
                            "noise_note",
                            "The noise filter turns down a room's steady noise in your microphone. It uses a little more battery.",
                        ),
                        Instant::now(),
                    ));
                }
            }
            Action::Voices(volume) => self.voice.set_voices_volume(volume),
            Action::Volume(uuid, volume) => self.voice.set_volume(uuid, volume),
            Action::Mute(uuid, muted) => self.voice.set_muted(uuid, muted),
            Action::Page(first) => self.voice.first = first,
            Action::Tab(tab) => {
                self.voice.tab = tab;
                self.voice.create_mode = false;
                self.voice.join_password_for = None;
                self.voice.focused_field = None;
                self.voice.groups_first = 0;
            }
            Action::JoinGroup(id) => {
                let Some(group) = self.voice.svc.groups.get(&id).cloned() else {
                    return;
                };
                if group.has_password {
                    self.voice.join_password_for = Some((id, group.name));
                    self.voice.create_mode = false;
                    self.voice.password_input.clear();
                    self.voice.focused_field = Some(GroupField::Password);
                    self.voice.note = None;
                } else if let Some(sender) = &self.voice.sender {
                    sender.send_custom(CH_SET_GROUP, encode_join_group(id, None));
                }
            }
            Action::LeaveGroup => {
                if let Some(sender) = &self.voice.sender {
                    sender.send_custom(CH_LEAVE_GROUP, encode_leave_group());
                }
            }
            Action::OpenCreateGroup => {
                self.voice.create_mode = true;
                self.voice.join_password_for = None;
                self.voice.name_input.clear();
                self.voice.password_input.clear();
                self.voice.group_type = GroupType::Normal;
                self.voice.focused_field = Some(GroupField::Name);
                self.voice.note = None;
            }
            Action::CancelGroupForm => {
                self.voice.create_mode = false;
                self.voice.join_password_for = None;
                self.voice.password_input.clear();
                self.voice.focused_field = None;
            }
            Action::CycleGroupType => {
                self.voice.group_type = match self.voice.group_type {
                    GroupType::Normal => GroupType::Open,
                    GroupType::Open => GroupType::Isolated,
                    GroupType::Isolated => GroupType::Normal,
                };
            }
            Action::SubmitCreateGroup => {
                let name = self.voice.name_input.trim().to_owned();
                if name.is_empty() {
                    return;
                }
                let pw = (!self.voice.password_input.is_empty())
                    .then(|| self.voice.password_input.clone());
                if let Some(sender) = &self.voice.sender {
                    sender.send_custom(
                        CH_CREATE_GROUP,
                        encode_create_group(&name, pw.as_deref(), self.voice.group_type),
                    );
                }
            }
            Action::SubmitJoinPassword => {
                let Some((id, _)) = self.voice.join_password_for else {
                    return;
                };
                if self.voice.password_input.is_empty() {
                    return;
                }
                if let Some(sender) = &self.voice.sender {
                    sender.send_custom(
                        CH_SET_GROUP,
                        encode_join_group(id, Some(&self.voice.password_input)),
                    );
                }
            }
            Action::FocusField(field) => {
                self.voice.focused_field = Some(field);
            }
            Action::GroupPage(first) => {
                self.voice.groups_first = first;
            }
            Action::Back => {
                self.voice_page_close();
            }
        }
    }

    /// Who is talking now, a small list in the top left corner of the HUD (hidden with the
    /// rest of it, and behind the debug screen).
    pub(super) fn voice_hud(&self, batch: &mut HudBatch, corners: &mut Corners, visible: bool) {
        if !self.voice.enabled() || !visible {
            return;
        }
        let you = self.voice.mic_level().1.then(|| tr("you", "You"));
        let names: Vec<&String> = you.iter().chain(&self.voice.talkers).collect();
        if names.is_empty() {
            return;
        }
        let atlas = &self.gui.atlas;
        let width = names
            .iter()
            .map(|n| HudBatch::text_width(atlas, n))
            .fold(0.0f32, f32::max)
            + 12.0;
        let height = names.len() as f32 * 10.0 + 2.0;
        let (x, y) = corners.place(Corner::TopLeft, width, height);
        batch.fill(atlas, x, y, width, height, rgba(0, 0, 0, 120));
        for (i, name) in names.iter().enumerate() {
            let row = y + 1.0 + i as f32 * 10.0;
            batch.fill(atlas, x + 3.0, row + 3.0, 3.0, 3.0, rgba(85, 255, 85, 255));
            batch.text(atlas, name, x + 9.0, row, u32::MAX, true);
        }
    }

    /// The window is closing: what was chosen is written.
    pub(super) fn voice_save_now(&mut self) {
        self.voice.save(true);
    }

    /// `horizon-ctl voice ...`, for the end-to-end checks.
    pub(super) fn voice_control(&mut self, words: &[&str]) -> Result<Option<serde_json::Value>> {
        use serde_json::json;
        if words.first() != Some(&"voice") {
            return Ok(None);
        }
        match (words.get(1).copied(), words.get(2).copied()) {
            (Some("enable"), _) => self.voice_set_enabled(true),
            (Some("disable"), _) => self.voice_set_enabled(false),
            (Some("mic"), Some("on")) => self.voice_set_mic(true),
            (Some("mic"), Some("off")) => self.voice_set_mic(false),
            (Some("mic"), Some("toggle")) => self.voice_toggle_mic(),
            (Some("input"), name) => self.voice.set_mic_choice(name.unwrap_or("")),
            (Some("voices"), Some(volume)) => {
                let volume = volume.parse().map_err(|_| "voice voices 0..2")?;
                self.voice.set_voices_volume(volume);
            }
            // The checks' microphone: a tone of this pitch, from the next time it is live.
            (Some("tone"), Some(hz)) => {
                self.voice.tone = Some(hz.parse().map_err(|_| "voice tone HZ")?);
                self.voice.close_mic();
            }
            (Some("mute"), Some(name)) | (Some("unmute"), Some(name)) => {
                let muted = words.get(1) == Some(&"mute");
                let person = self
                    .voice
                    .people()
                    .into_iter()
                    .find(|p| p.name == name)
                    .ok_or("nobody of that name is in voice chat")?;
                self.voice.set_muted(person.uuid, muted);
            }
            (Some("volume"), Some(name)) => {
                let volume: f32 = words
                    .get(3)
                    .and_then(|v| v.parse().ok())
                    .ok_or("voice volume NAME 0..2")?;
                let person = self
                    .voice
                    .people()
                    .into_iter()
                    .find(|p| p.name == name)
                    .ok_or("nobody of that name is in voice chat")?;
                self.voice.set_volume(person.uuid, volume);
            }
            (Some("status"), _) => {}
            _ => {
                return Err(
                    "voice enable|disable|mic on|off|toggle|input [NAME]|voices V|tone HZ|mute NAME|unmute NAME|volume NAME V|status"
                        .into(),
                );
            }
        }
        let heard = self
            .voice
            .probe
            .as_ref()
            .map(|p| *p.heard.lock().unwrap_or_else(|e| e.into_inner()))
            .unwrap_or_default();
        let (mic_level, speaking) = self.voice.mic_level();
        Ok(Some(json!({
            "offered": self.voice_offered(),
            "connected": self.voice.connected(),
            "enabled": self.voice.enabled(),
            "mic": self.voice.mic_live(),
            "speaking": speaking,
            "mic_level": mic_level,
            "input": self.voice.mic_choice(),
            "voices": self.voice.voices_volume(),
            "people": self.voice.people().iter().map(|p| json!({
                "name": p.name,
                "enabled": p.enabled,
                "mic": p.mic_live,
                "volume": p.volume,
                "muted": p.muted,
                "talking": p.talking,
                "level": p.level,
                "delay_ms": p.delay_ms,
            })).collect::<Vec<_>>(),
            "heard": {
                "level": heard.level,
                "hz": heard.hz,
                "blocks": heard.blocks,
                "quiet_blocks": heard.quiet_blocks,
            },
            "lines": self.voice.lines,
        })))
    }
}
