//! Simple Voice Chat encrypted UDP client (`horizon_voice::svc`).
//!
//! Runs a dedicated background thread (`horizon-svc-udp`) that resolves the server's UDP
//! endpoint, completes the `Authenticate -> AuthenticateAck -> ConnectionCheck -> ConnectionCheckAck`
//! handshake, maintains keep-alives/pings, and feeds incoming Opus frames straight into
//! `horizon_voice::engine::Playout` while recording per-speaker spatial metadata for the game
//! thread to apply 3D proximity attenuation and stereo panning.
use horizon_voice::{
    engine::Playout,
    svc::{
        SecretInfo, SvcSecret, SvcVersion, UdpPacket, decode_server_udp, encode_client_udp,
        parse_voice_host,
    },
    wire::{END, START},
};
use std::{
    collections::HashMap,
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// How a speaker slot in `Playout` should be spatialized on the next frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpatialSource {
    Player {
        uuid: u128,
        distance: f32,
        whispering: bool,
    },
    Group {
        uuid: u128,
    },
    Location {
        pos: [f64; 3],
        distance: f32,
    },
}

/// Shared speaker slot table (`uuid/channel -> u8 Playout ID`) & per-slot spatial state.
#[derive(Default)]
struct Slots {
    by_key: HashMap<u128, u8>,
    spatial: HashMap<u8, SpatialSource>,
    last_seq: HashMap<u8, i64>,
    next_id: u8,
}

impl Slots {
    fn slot_for(&mut self, key: u128) -> u8 {
        if let Some(&id) = self.by_key.get(&key) {
            return id;
        }
        // Allocate 1..=254, reusing the oldest/wrap slot if > 254 unique keys ever appear.
        if self.next_id == 0 || self.next_id == 255 {
            self.next_id = 1;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.by_key.retain(|_, v| *v != id);
        self.by_key.insert(key, id);
        id
    }
}

/// Thread-safe handle for sending outbound microphone Opus packets over UDP.
pub struct SvcMicSender {
    socket: UdpSocket,
    target: SocketAddr,
    version: SvcVersion,
    routing_id: u128,
    secret: SvcSecret,
    connected: Arc<AtomicBool>,
}

impl SvcMicSender {
    /// Sends one Opus frame (`opus` empty = end-of-speech marker in SVC).
    pub fn send_mic(&self, seq: i64, opus: &[u8], whispering: bool) {
        if !self.connected.load(Ordering::SeqCst) {
            return;
        }
        let packet = UdpPacket::Mic {
            opus: opus.to_vec(),
            seq,
            whispering,
        };
        if let Ok(bytes) = encode_client_udp(self.version, self.routing_id, &self.secret, &packet) {
            let _ = self.socket.send_to(&bytes, self.target);
        }
    }
}

/// Active Simple Voice Chat UDP connection. Dropping it stops the background worker thread.
pub struct SvcUdpClient {
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    failed: Arc<Mutex<Option<String>>>,
    slots: Arc<Mutex<Slots>>,
    mic_sender: Arc<SvcMicSender>,
    thread: Option<JoinHandle<()>>,
}

impl SvcUdpClient {
    /// Resolves the SVC UDP endpoint and starts the handshake + receive loop thread.
    pub fn connect(
        info: SecretInfo,
        server_ip: &str,
        playout: Arc<Playout>,
    ) -> Result<Self, String> {
        let (host, port) = parse_voice_host(&info.voice_host, server_ip, info.server_port);
        let target = resolve_udp(&host, port)?;
        let bind_addr = if target.is_ipv6() {
            "[::]:0"
        } else {
            "0.0.0.0:0"
        };
        let socket = UdpSocket::bind(bind_addr)
            .map_err(|e| format!("bind UDP socket ({bind_addr}): {e}"))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .map_err(|e| format!("set UDP read timeout: {e}"))?;
        let send_socket = socket
            .try_clone()
            .map_err(|e| format!("clone UDP socket: {e}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(Mutex::new(None));
        let slots = Arc::new(Mutex::new(Slots::default()));

        let mic_sender = Arc::new(SvcMicSender {
            socket: send_socket,
            target,
            version: info.version,
            routing_id: info.connection_id,
            secret: info.secret,
            connected: connected.clone(),
        });

        let thread = {
            let stop = stop.clone();
            let connected = connected.clone();
            let failed = failed.clone();
            let slots = slots.clone();
            std::thread::Builder::new()
                .name("horizon-svc-udp".into())
                .spawn(move || {
                    run_udp(
                        socket, target, info, playout, stop, connected, failed, slots,
                    );
                })
                .map_err(|e| format!("spawn SVC UDP thread: {e}"))?
        };

        Ok(Self {
            stop,
            connected,
            failed,
            slots,
            mic_sender,
            thread: Some(thread),
        })
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    pub fn failure(&self) -> Option<String> {
        self.failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn mic_sender(&self) -> Arc<SvcMicSender> {
        self.mic_sender.clone()
    }

    /// Returns the `Playout` speaker ID assigned to a player UUID (assigning one if needed).
    pub fn slot_for(&self, uuid: u128) -> u8 {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slot_for(uuid)
    }

    /// Snapshot of active speaker slots and their most recent spatial metadata.
    pub fn spatial_snapshot(&self) -> Vec<(u8, SpatialSource)> {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .spatial
            .iter()
            .map(|(&id, &src)| (id, src))
            .collect()
    }
}

impl Drop for SvcUdpClient {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn resolve_udp(host: &str, port: u16) -> Result<SocketAddr, String> {
    let host = if host == "rtc-local" || host == "rtc-remote" || host == "local" || host.is_empty()
    {
        "127.0.0.1"
    } else {
        host
    };
    let mut addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}:{port}: {e}"))?;
    let all: Vec<SocketAddr> = addrs.by_ref().collect();
    // Prefer IPv4 when both exist (most Minecraft SVC servers bind 0.0.0.0).
    all.iter()
        .copied()
        .find(SocketAddr::is_ipv4)
        .or_else(|| all.first().copied())
        .ok_or_else(|| format!("no address for {host}:{port}"))
}

#[allow(clippy::too_many_arguments)]
fn run_udp(
    socket: UdpSocket,
    target: SocketAddr,
    info: SecretInfo,
    playout: Arc<Playout>,
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    failed: Arc<Mutex<Option<String>>>,
    slots: Arc<Mutex<Slots>>,
) {
    let mut authenticated = false;
    let mut check_nonce: Option<[u8; 16]> = None;
    let mut last_auth_try = Instant::now() - Duration::from_secs(5);
    let mut last_keep_alive = Instant::now();
    let timeout = Duration::from_millis(u64::from(info.keep_alive_ms).saturating_mul(10).max(5000));
    let mut buf = [0u8; 4096];

    let send_pkt = |pkt: &UdpPacket| {
        if let Ok(bytes) = encode_client_udp(info.version, info.connection_id, &info.secret, pkt) {
            let _ = socket.send_to(&bytes, target);
        }
    };

    while !stop.load(Ordering::SeqCst) {
        if !connected.load(Ordering::SeqCst) && last_auth_try.elapsed() >= Duration::from_secs(1) {
            last_auth_try = Instant::now();
            if !authenticated {
                send_pkt(&UdpPacket::Authenticate {
                    player_uuid: info.player_uuid,
                    secret: info.secret,
                });
            } else {
                send_pkt(&UdpPacket::ConnectionCheck { nonce: check_nonce });
            }
        }

        if connected.load(Ordering::SeqCst) && last_keep_alive.elapsed() > timeout {
            connected.store(false, Ordering::SeqCst);
            *failed.lock().unwrap_or_else(|e| e.into_inner()) =
                Some("Voice chat UDP connection timed out".into());
            break;
        }

        let n = match socket.recv_from(&mut buf) {
            Ok((n, _from)) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
        };

        let Some(pkt) = decode_server_udp(info.version, &info.secret, &buf[..n]) else {
            continue;
        };

        match pkt {
            UdpPacket::AuthenticateAck { nonce } => {
                if !authenticated {
                    authenticated = true;
                    check_nonce = nonce;
                    last_auth_try = Instant::now();
                    send_pkt(&UdpPacket::ConnectionCheck { nonce: check_nonce });
                }
            }
            UdpPacket::ConnectionCheckAck => {
                if !connected.swap(true, Ordering::SeqCst) {
                    last_keep_alive = Instant::now();
                    log::info!(
                        "Simple Voice Chat connected ({:?}) to {target}",
                        info.version
                    );
                }
            }
            UdpPacket::KeepAlive => {
                last_keep_alive = Instant::now();
                send_pkt(&UdpPacket::KeepAlive);
            }
            UdpPacket::Ping { id, timestamp } => {
                send_pkt(&UdpPacket::Ping { id, timestamp });
            }
            UdpPacket::PlayerSound {
                sender,
                opus,
                seq,
                distance,
                whispering,
                ..
            } => {
                push_sound(
                    &slots,
                    &playout,
                    sender,
                    seq,
                    &opus,
                    SpatialSource::Player {
                        uuid: sender,
                        distance,
                        whispering,
                    },
                );
            }
            UdpPacket::GroupSound {
                sender, opus, seq, ..
            } => {
                push_sound(
                    &slots,
                    &playout,
                    sender,
                    seq,
                    &opus,
                    SpatialSource::Group { uuid: sender },
                );
            }
            UdpPacket::LocationSound {
                channel_id,
                location,
                opus,
                seq,
                distance,
                ..
            } => {
                push_sound(
                    &slots,
                    &playout,
                    channel_id,
                    seq,
                    &opus,
                    SpatialSource::Location {
                        pos: location,
                        distance,
                    },
                );
            }
            UdpPacket::Mic { .. }
            | UdpPacket::Authenticate { .. }
            | UdpPacket::ConnectionCheck { .. } => {}
        }
    }
}

fn push_sound(
    slots: &Arc<Mutex<Slots>>,
    playout: &Playout,
    key: u128,
    seq: i64,
    opus: &[u8],
    source: SpatialSource,
) {
    let (slot, flags) = {
        let mut s = slots.lock().unwrap_or_else(|e| e.into_inner());
        let slot = s.slot_for(key);
        s.spatial.insert(slot, source);
        let prev = s.last_seq.insert(slot, seq);
        let flags = if opus.is_empty() {
            s.last_seq.remove(&slot);
            END
        } else if prev.is_none_or(|p| seq <= p || seq - p > 10) {
            START
        } else {
            0
        };
        (slot, flags)
    };
    playout.push(slot, seq as u16, flags, opus);
}

#[cfg(test)]
mod tests {
    use super::*;
    use horizon_voice::{
        codec::{Encoder, MAX_PACKET},
        svc::{SvcCodec, decode_udp_inner, encode_server_udp},
    };

    fn run_loopback_handshake(version: SvcVersion) {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        server
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();

        let secret = SvcSecret(*b"abcdef0123456789");
        let player_uuid = 0x1111_2222_3333_4444_5555_6666_7777_8888u128;
        let conn_id = if version == SvcVersion::V21 {
            0x9999_aaaa_bbbb_cccc_dddd_eeee_ffff_0001u128
        } else {
            player_uuid
        };
        let nonce = [0x5Au8; 16];

        let info = SecretInfo {
            version,
            secret,
            connection_id: conn_id,
            server_port: port,
            player_uuid,
            codec: SvcCodec::Voip,
            mtu_size: 1024,
            voice_distance: 48.0,
            keep_alive_ms: 1000,
            groups_enabled: true,
            voice_host: String::new(),
            allow_recording: false,
        };

        let server_thread = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            // 1. Receive Authenticate
            let (n, peer) = server.recv_from(&mut buf).unwrap();
            assert_eq!(buf[0], horizon_voice::svc::MAGIC_BYTE);
            let got_id = u128::from_be_bytes(buf[1..17].try_into().unwrap());
            assert_eq!(got_id, conn_id);
            let inner = decrypt_client_dgram(version, &secret, &buf[17..n]);
            assert!(matches!(
                decode_udp_inner(version, &inner),
                Some(UdpPacket::Authenticate { .. })
            ));
            // 2. Reply AuthenticateAck
            let ack = encode_server_udp(
                version,
                &secret,
                &UdpPacket::AuthenticateAck {
                    nonce: (version == SvcVersion::V21).then_some(nonce),
                },
            )
            .unwrap();
            server.send_to(&ack, peer).unwrap();

            // 3. Receive ConnectionCheck
            let (n, peer) = server.recv_from(&mut buf).unwrap();
            let inner = decrypt_client_dgram(version, &secret, &buf[17..n]);
            match decode_udp_inner(version, &inner) {
                Some(UdpPacket::ConnectionCheck { nonce: got }) => {
                    if version == SvcVersion::V21 {
                        assert_eq!(got, Some(nonce));
                    }
                }
                other => panic!("expected ConnectionCheck, got {other:?}"),
            }
            // 4. Reply ConnectionCheckAck
            let cack = encode_server_udp(version, &secret, &UdpPacket::ConnectionCheckAck).unwrap();
            server.send_to(&cack, peer).unwrap();

            // 5. Send one real Opus PlayerSoundPacket
            let mut enc = Encoder::new().unwrap();
            let mut opus = [0u8; MAX_PACKET];
            let pcm = [0.1f32; horizon_voice::FRAME];
            let len = enc.encode(&pcm, &mut opus).unwrap();
            let sound = encode_server_udp(
                version,
                &secret,
                &UdpPacket::PlayerSound {
                    channel_id: 42,
                    sender: 42,
                    opus: opus[..len].to_vec(),
                    seq: 1,
                    distance: 48.0,
                    whispering: false,
                    category: None,
                },
            )
            .unwrap();
            server.send_to(&sound, peer).unwrap();
        });

        let playout = Arc::new(Playout::default());
        let client = SvcUdpClient::connect(info, "127.0.0.1", playout.clone()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while (!client.is_connected() || !playout.live()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(client.is_connected());
        assert!(playout.live());
        let spatial = client.spatial_snapshot();
        assert_eq!(spatial.len(), 1);
        assert!(matches!(
            spatial[0].1,
            SpatialSource::Player { uuid: 42, .. }
        ));
        server_thread.join().unwrap();
    }

    fn decrypt_client_dgram(version: SvcVersion, secret: &SvcSecret, tail: &[u8]) -> Vec<u8> {
        let mut pos = 0usize;
        let mut len = 0u32;
        let mut shift = 0u32;
        loop {
            let b = tail[pos];
            pos += 1;
            len |= u32::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        secret
            .decrypt(version, &tail[pos..pos + len as usize])
            .unwrap()
    }

    #[test]
    fn loopback_handshake_v21_and_v18() {
        run_loopback_handshake(SvcVersion::V21);
        run_loopback_handshake(SvcVersion::V18);
    }
}
