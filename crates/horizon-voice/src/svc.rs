//! Simple Voice Chat (`henkelmax/simple-voice-chat`) wire protocol and crypto codec.
//!
//! Supports protocol compatibility versions **21** (SVC 2.7.x), **20** (SVC 2.6.x), and
//! **18** (SVC 2.5.x), covering both TCP Minecraft plugin channels (`voicechat:*`) and
//! AES-encrypted UDP voice packets.
use aes::Aes128;
use aes_gcm::{
    Aes128Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};

type Aes128CbcEnc = cbc::Encryptor<Aes128>;
type Aes128CbcDec = cbc::Decryptor<Aes128>;

/// Magic first byte of every Simple Voice Chat UDP datagram.
pub const MAGIC_BYTE: u8 = 0xFF;
/// Length of the AES-128 key exchanged in `voicechat:secret`.
pub const SECRET_BYTES: usize = 16;
/// Length of the connection check nonce in v21.
pub const NONCE_BYTES: usize = 16;
/// Maximum UDP encrypted payload accepted from the network.
pub const MAX_UDP_PAYLOAD: usize = 4096;
/// Maximum Opus payload length in sound/mic packets.
pub const MAX_OPUS_PAYLOAD: usize = 1024;
/// Maximum group name length enforced by SVC.
pub const MAX_GROUP_NAME: usize = 24;
/// Maximum group password length enforced by SVC.
pub const MAX_GROUP_PASSWORD: usize = 256;

/// Prefix of every Simple Voice Chat Minecraft plugin channel.
pub const CHANNEL_PREFIX: &str = "voicechat:";
pub const CH_REQUEST_SECRET: &str = "voicechat:request_secret";
pub const CH_SECRET: &str = "voicechat:secret";
pub const CH_STATE: &str = "voicechat:state";
pub const CH_STATES: &str = "voicechat:states";
pub const CH_REMOVE_STATE: &str = "voicechat:remove_state";
pub const CH_CLIENT_STATE: &str = "voicechat:client_state";
pub const CH_LEGACY_PLAYER_STATE: &str = "voicechat:player_state";
pub const CH_LEGACY_PLAYER_STATES: &str = "voicechat:player_states";
pub const CH_LEGACY_UPDATE_STATE: &str = "voicechat:update_state";
pub const CH_ADD_GROUP: &str = "voicechat:add_group";
pub const CH_REMOVE_GROUP: &str = "voicechat:remove_group";
pub const CH_JOINED_GROUP: &str = "voicechat:joined_group";
pub const CH_SET_GROUP: &str = "voicechat:set_group";
pub const CH_CREATE_GROUP: &str = "voicechat:create_group";
pub const CH_LEAVE_GROUP: &str = "voicechat:leave_group";

/// UDP inner packet type IDs (`NetworkMessage.packetRegistry`).
pub const PKT_MIC: u8 = 0x01;
pub const PKT_PLAYER_SOUND: u8 = 0x02;
pub const PKT_GROUP_SOUND: u8 = 0x03;
pub const PKT_LOCATION_SOUND: u8 = 0x04;
pub const PKT_AUTHENTICATE: u8 = 0x05;
pub const PKT_AUTHENTICATE_ACK: u8 = 0x06;
pub const PKT_PING: u8 = 0x07;
pub const PKT_KEEP_ALIVE: u8 = 0x08;
pub const PKT_CONNECTION_CHECK: u8 = 0x09;
pub const PKT_CONNECTION_CHECK_ACK: u8 = 0x0A;

const SOUND_WHISPER_MASK: u8 = 0b01;
const SOUND_HAS_CATEGORY_MASK: u8 = 0b10;

/// Supported Simple Voice Chat compatibility versions, probed newest-first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvcVersion {
    /// SVC 2.7.x (`AES-128-GCM`, `connectionId` in `SecretPacket` & UDP header, 16-byte nonce).
    V21,
    /// SVC 2.6.x (`AES-128-GCM`, `playerUUID` in UDP header, no `connectionId` or nonce).
    V20,
    /// SVC 2.5.x (`AES-128-CBC/PKCS5`, `playerUUID` in UDP header, legacy TCP channels).
    V18,
}

impl SvcVersion {
    /// Order in which versions are probed on an unknown server.
    pub const PROBE_ORDER: [Self; 3] = [Self::V21, Self::V20, Self::V18];

    pub const fn compat_id(self) -> i32 {
        match self {
            Self::V21 => 21,
            Self::V20 => 20,
            Self::V18 => 18,
        }
    }
}

/// Server-selected Opus application mode from `SecretPacket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SvcCodec {
    Voip,
    Audio,
    RestrictedLowdelay,
}

impl SvcCodec {
    fn from_byte(b: u8) -> Self {
        match b {
            1 => Self::Audio,
            2 => Self::RestrictedLowdelay,
            _ => Self::Voip,
        }
    }
}

/// Group visibility/isolation mode (`Group.Type` / `GroupImpl.TypeImpl`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GroupType {
    #[default]
    Normal,
    Open,
    Isolated,
}

impl GroupType {
    pub const fn to_short(self) -> i16 {
        match self {
            Self::Normal => 0,
            Self::Open => 1,
            Self::Isolated => 2,
        }
    }

    pub const fn from_short(v: i16) -> Self {
        match v {
            1 => Self::Open,
            2 => Self::Isolated,
            _ => Self::Normal,
        }
    }

    pub const fn next(self) -> Self {
        match self {
            Self::Normal => Self::Open,
            Self::Open => Self::Isolated,
            Self::Isolated => Self::Normal,
        }
    }
}

/// 16-byte AES-128 key negotiated over TCP in `voicechat:secret`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SvcSecret(pub [u8; SECRET_BYTES]);

impl std::fmt::Debug for SvcSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SvcSecret([redacted])")
    }
}

impl SvcSecret {
    /// Encrypts `plaintext` according to `version` (`AES-128-GCM` for v20/v21, `AES-128-CBC/PKCS7` for v18).
    pub fn encrypt(&self, version: SvcVersion, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        match version {
            SvcVersion::V21 | SvcVersion::V20 => {
                let mut iv = [0u8; 12];
                getrandom::fill(&mut iv).map_err(|e| format!("random IV: {e}"))?;
                let cipher =
                    Aes128Gcm::new_from_slice(&self.0).map_err(|e| format!("AES-GCM key: {e}"))?;
                let ct = cipher
                    .encrypt(Nonce::from_slice(&iv), plaintext)
                    .map_err(|e| format!("AES-GCM encrypt: {e}"))?;
                let mut out = Vec::with_capacity(iv.len() + ct.len());
                out.extend_from_slice(&iv);
                out.extend_from_slice(&ct);
                Ok(out)
            }
            SvcVersion::V18 => {
                let mut iv = [0u8; 16];
                getrandom::fill(&mut iv).map_err(|e| format!("random IV: {e}"))?;
                let ct = Aes128CbcEnc::new(&self.0.into(), &iv.into())
                    .encrypt_padded_vec_mut::<Pkcs7>(plaintext);
                let mut out = Vec::with_capacity(iv.len() + ct.len());
                out.extend_from_slice(&iv);
                out.extend_from_slice(&ct);
                Ok(out)
            }
        }
    }

    /// Decrypts `payload` according to `version`.
    pub fn decrypt(&self, version: SvcVersion, payload: &[u8]) -> Result<Vec<u8>, String> {
        match version {
            SvcVersion::V21 | SvcVersion::V20 => {
                if payload.len() < 12 + 16 {
                    return Err("AES-GCM payload too short".into());
                }
                let (iv, ct) = payload.split_at(12);
                let cipher =
                    Aes128Gcm::new_from_slice(&self.0).map_err(|e| format!("AES-GCM key: {e}"))?;
                cipher
                    .decrypt(Nonce::from_slice(iv), ct)
                    .map_err(|e| format!("AES-GCM decrypt: {e}"))
            }
            SvcVersion::V18 => {
                if payload.len() < 32 || !(payload.len() - 16).is_multiple_of(16) {
                    return Err("AES-CBC payload invalid length".into());
                }
                let (iv, ct) = payload.split_at(16);
                Aes128CbcDec::new(&self.0.into(), iv.into())
                    .decrypt_padded_vec_mut::<Pkcs7>(ct)
                    .map_err(|e| format!("AES-CBC decrypt: {e}"))
            }
        }
    }
}

/// Decoded `voicechat:secret` TCP handshake response.
#[derive(Debug, Clone, PartialEq)]
pub struct SecretInfo {
    pub version: SvcVersion,
    pub secret: SvcSecret,
    /// Separate UDP routing ID on v21; equals `player_uuid` on v18/v20.
    pub connection_id: u128,
    pub server_port: u16,
    pub player_uuid: u128,
    pub codec: SvcCodec,
    pub mtu_size: i32,
    pub voice_distance: f64,
    pub keep_alive_ms: u32,
    pub groups_enabled: bool,
    pub voice_host: String,
    pub allow_recording: bool,
}

/// Remote player's voice state (`PlayerState`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerState {
    pub uuid: u128,
    pub name: String,
    pub muted: bool,
    pub force_muted: bool,
    pub disabled: bool,
    pub force_disabled: bool,
    pub disconnected: bool,
    pub group: Option<u128>,
}

/// Voice chat group (`ClientGroup`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientGroup {
    pub id: u128,
    pub name: String,
    pub has_password: bool,
    pub persistent: bool,
    pub hidden: bool,
    pub kind: GroupType,
}

/// Decoded inbound TCP `voicechat:*` plugin message.
#[derive(Debug, Clone, PartialEq)]
pub enum TcpIn {
    Secret(SecretInfo),
    PlayerState(PlayerState),
    PlayerStates(Vec<PlayerState>),
    RemovePlayerState(u128),
    AddGroup(ClientGroup),
    RemoveGroup(u128),
    JoinedGroup {
        group: Option<u128>,
        wrong_password: bool,
    },
}

/// Decoded UDP voice packet (inner payload after decryption).
#[derive(Debug, Clone, PartialEq)]
pub enum UdpPacket {
    Mic {
        opus: Vec<u8>,
        seq: i64,
        whispering: bool,
    },
    PlayerSound {
        channel_id: u128,
        sender: u128,
        opus: Vec<u8>,
        seq: i64,
        distance: f32,
        whispering: bool,
        category: Option<String>,
    },
    GroupSound {
        channel_id: u128,
        sender: u128,
        opus: Vec<u8>,
        seq: i64,
        category: Option<String>,
    },
    LocationSound {
        channel_id: u128,
        sender: u128,
        location: [f64; 3],
        opus: Vec<u8>,
        seq: i64,
        distance: f32,
        category: Option<String>,
    },
    Authenticate {
        /// Only written on v18/v20 (empty on v21).
        player_uuid: u128,
        secret: SvcSecret,
    },
    AuthenticateAck {
        /// 16-byte nonce on v21; empty on v18/v20.
        nonce: Option<[u8; NONCE_BYTES]>,
    },
    Ping {
        id: u128,
        timestamp: i64,
    },
    KeepAlive,
    ConnectionCheck {
        /// 16-byte nonce on v21; empty on v18/v20.
        nonce: Option<[u8; NONCE_BYTES]>,
    },
    ConnectionCheckAck,
}

// --- Minecraft FriendlyByteBuf helpers ---

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        if end > self.buf.len() {
            return None;
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn bool(&mut self) -> Option<bool> {
        Some(self.u8()? != 0)
    }

    fn i16(&mut self) -> Option<i16> {
        Some(i16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn uuid(&mut self) -> Option<u128> {
        Some(u128::from_be_bytes(self.take(16)?.try_into().ok()?))
    }

    fn varint(&mut self) -> Option<i32> {
        let mut value = 0u32;
        let mut shift = 0u32;
        for _ in 0..5 {
            let b = self.u8()?;
            value |= u32::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                return Some(value as i32);
            }
            shift += 7;
        }
        None
    }

    fn byte_array(&mut self, max_len: usize) -> Option<Vec<u8>> {
        let len = self.varint()?;
        if len < 0 || len as usize > max_len {
            return None;
        }
        Some(self.take(len as usize)?.to_vec())
    }

    fn utf(&mut self, max_chars: usize) -> Option<String> {
        let bytes = self.byte_array(max_chars.saturating_mul(4))?;
        let s = String::from_utf8(bytes).ok()?;
        if s.chars().count() > max_chars {
            return None;
        }
        Some(s)
    }
}

fn write_varint(out: &mut Vec<u8>, v: i32) {
    let mut u = v as u32;
    loop {
        let mut b = (u & 0x7F) as u8;
        u >>= 7;
        if u != 0 {
            b |= 0x80;
        }
        out.push(b);
        if u == 0 {
            break;
        }
    }
}

fn write_byte_array(out: &mut Vec<u8>, bytes: &[u8]) {
    write_varint(out, bytes.len() as i32);
    out.extend_from_slice(bytes);
}

fn write_utf(out: &mut Vec<u8>, s: &str) {
    write_byte_array(out, s.as_bytes());
}

fn write_uuid(out: &mut Vec<u8>, uuid: u128) {
    out.extend_from_slice(&uuid.to_be_bytes());
}

// --- TCP Encoding / Decoding ---

/// Encodes `voicechat:request_secret` for `version`.
pub fn encode_request_secret(version: SvcVersion) -> Vec<u8> {
    version.compat_id().to_be_bytes().to_vec()
}

/// Encodes client state update (`voicechat:client_state` for v20/v21, `voicechat:update_state` for v18).
pub fn encode_update_state(
    version: SvcVersion,
    muted: bool,
    disabled: bool,
) -> (&'static str, Vec<u8>) {
    match version {
        SvcVersion::V18 => (CH_LEGACY_UPDATE_STATE, vec![u8::from(disabled)]),
        SvcVersion::V20 | SvcVersion::V21 => {
            (CH_CLIENT_STATE, vec![u8::from(muted), u8::from(disabled)])
        }
    }
}

/// Encodes `voicechat:set_group` (`JoinGroupPacket`).
pub fn encode_join_group(group: u128, password: Option<&str>) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    write_uuid(&mut out, group);
    let pw = password.filter(|s| !s.is_empty());
    out.push(u8::from(pw.is_some()));
    if let Some(p) = pw {
        write_utf(&mut out, p);
    }
    out
}

/// Encodes `voicechat:create_group` (`CreateGroupPacket`).
pub fn encode_create_group(name: &str, password: Option<&str>, kind: GroupType) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    let trimmed: String = name.trim().chars().take(MAX_GROUP_NAME).collect();
    write_utf(&mut out, &trimmed);
    let pw = password.filter(|s| !s.is_empty());
    out.push(u8::from(pw.is_some()));
    if let Some(p) = pw {
        let p_trim: String = p.chars().take(MAX_GROUP_PASSWORD).collect();
        write_utf(&mut out, &p_trim);
    }
    out.extend_from_slice(&kind.to_short().to_be_bytes());
    out
}

/// Encodes `voicechat:leave_group` (`LeaveGroupPacket`).
pub fn encode_leave_group() -> Vec<u8> {
    Vec::new()
}

fn decode_player_state(r: &mut Reader<'_>, v21_flags: bool) -> Option<PlayerState> {
    if v21_flags {
        let flags = r.u8()?;
        let muted = flags & (1 << 0) != 0;
        let force_muted = flags & (1 << 1) != 0;
        let disabled = flags & (1 << 2) != 0;
        let force_disabled = flags & (1 << 3) != 0;
        let disconnected = flags & (1 << 4) != 0;
        let has_group = flags & (1 << 5) != 0;
        let uuid = r.uuid()?;
        let name = r.utf(256)?;
        let group = if has_group { Some(r.uuid()?) } else { None };
        Some(PlayerState {
            uuid,
            name,
            muted,
            force_muted,
            disabled,
            force_disabled,
            disconnected,
            group,
        })
    } else {
        let disabled = r.bool()?;
        let disconnected = r.bool()?;
        let uuid = r.uuid()?;
        let name = r.utf(256)?;
        let group = if r.bool()? { Some(r.uuid()?) } else { None };
        Some(PlayerState {
            uuid,
            name,
            muted: false,
            force_muted: false,
            disabled,
            force_disabled: false,
            disconnected,
            group,
        })
    }
}

fn decode_group(r: &mut Reader<'_>) -> Option<ClientGroup> {
    let id = r.uuid()?;
    let name = r.utf(MAX_GROUP_NAME)?;
    let has_password = r.bool()?;
    let persistent = r.bool()?;
    let hidden = r.bool()?;
    let kind = GroupType::from_short(r.i16()?);
    Some(ClientGroup {
        id,
        name,
        has_password,
        persistent,
        hidden,
        kind,
    })
}

/// Decodes `voicechat:secret` given the `version` that was probed/negotiated.
pub fn decode_secret(version: SvcVersion, data: &[u8]) -> Option<SecretInfo> {
    let mut r = Reader::new(data);
    let secret_bytes: [u8; SECRET_BYTES] = r.take(SECRET_BYTES)?.try_into().ok()?;
    let connection_id_v21 = if version == SvcVersion::V21 {
        Some(r.uuid()?)
    } else {
        None
    };
    let port_i32 = r.i32()?;
    let server_port = u16::try_from(port_i32).ok().filter(|&p| p > 0)?;
    let player_uuid = r.uuid()?;
    let codec = SvcCodec::from_byte(r.u8()?);
    let mtu_size = r.i32()?.clamp(256, 1024);
    let voice_distance = r.f64()?;
    let keep_alive_ms = u32::try_from(r.i32()?).ok().unwrap_or(1000).max(200);
    let groups_enabled = r.bool()?;
    let voice_host = r.utf(32767)?;
    let allow_recording = r.bool()?;
    if r.remaining() != 0 {
        return None;
    }
    Some(SecretInfo {
        version,
        secret: SvcSecret(secret_bytes),
        connection_id: connection_id_v21.unwrap_or(player_uuid),
        server_port,
        player_uuid,
        codec,
        mtu_size,
        voice_distance,
        keep_alive_ms,
        groups_enabled,
        voice_host,
        allow_recording,
    })
}

/// Decodes any inbound `voicechat:*` TCP `CustomPayload`.
pub fn decode_tcp(version: SvcVersion, channel: &str, data: &[u8]) -> Option<TcpIn> {
    let v21_flags = version == SvcVersion::V21;
    match channel {
        CH_SECRET => Some(TcpIn::Secret(decode_secret(version, data)?)),
        CH_STATE => {
            let mut r = Reader::new(data);
            Some(TcpIn::PlayerState(decode_player_state(&mut r, v21_flags)?))
        }
        CH_LEGACY_PLAYER_STATE => {
            let mut r = Reader::new(data);
            Some(TcpIn::PlayerState(decode_player_state(&mut r, false)?))
        }
        CH_STATES => {
            let mut r = Reader::new(data);
            let count = usize::try_from(r.i32()?).ok()?.min(4096);
            let mut list = Vec::with_capacity(count);
            for _ in 0..count {
                list.push(decode_player_state(&mut r, v21_flags)?);
            }
            Some(TcpIn::PlayerStates(list))
        }
        CH_LEGACY_PLAYER_STATES => {
            let mut r = Reader::new(data);
            let count = usize::try_from(r.i32()?).ok()?.min(4096);
            let mut list = Vec::with_capacity(count);
            for _ in 0..count {
                list.push(decode_player_state(&mut r, false)?);
            }
            Some(TcpIn::PlayerStates(list))
        }
        CH_REMOVE_STATE => {
            let mut r = Reader::new(data);
            Some(TcpIn::RemovePlayerState(r.uuid()?))
        }
        CH_ADD_GROUP => {
            let mut r = Reader::new(data);
            Some(TcpIn::AddGroup(decode_group(&mut r)?))
        }
        CH_REMOVE_GROUP => {
            let mut r = Reader::new(data);
            Some(TcpIn::RemoveGroup(r.uuid()?))
        }
        CH_JOINED_GROUP => {
            let mut r = Reader::new(data);
            let group = if r.bool()? { Some(r.uuid()?) } else { None };
            let wrong_password = r.bool()?;
            Some(TcpIn::JoinedGroup {
                group,
                wrong_password,
            })
        }
        _ => None,
    }
}

// --- UDP Encoding / Decoding ---

/// Encodes an inner UDP packet (packet type byte + fields) without encryption.
pub fn encode_udp_inner(version: SvcVersion, packet: &UdpPacket) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    match packet {
        UdpPacket::Mic {
            opus,
            seq,
            whispering,
        } => {
            out.push(PKT_MIC);
            write_byte_array(&mut out, opus);
            out.extend_from_slice(&seq.to_be_bytes());
            out.push(u8::from(*whispering));
        }
        UdpPacket::Authenticate {
            player_uuid,
            secret,
        } => {
            out.push(PKT_AUTHENTICATE);
            if version != SvcVersion::V21 {
                write_uuid(&mut out, *player_uuid);
                out.extend_from_slice(&secret.0);
            }
        }
        UdpPacket::ConnectionCheck { nonce } => {
            out.push(PKT_CONNECTION_CHECK);
            if version == SvcVersion::V21
                && let Some(n) = nonce
            {
                out.extend_from_slice(n);
            }
        }
        UdpPacket::KeepAlive => out.push(PKT_KEEP_ALIVE),
        UdpPacket::Ping { id, timestamp } => {
            out.push(PKT_PING);
            write_uuid(&mut out, *id);
            out.extend_from_slice(&timestamp.to_be_bytes());
        }
        UdpPacket::AuthenticateAck { nonce } => {
            out.push(PKT_AUTHENTICATE_ACK);
            if version == SvcVersion::V21
                && let Some(n) = nonce
            {
                out.extend_from_slice(n);
            }
        }
        UdpPacket::ConnectionCheckAck => out.push(PKT_CONNECTION_CHECK_ACK),
        UdpPacket::PlayerSound {
            channel_id,
            sender,
            opus,
            seq,
            distance,
            whispering,
            category,
        } => {
            out.push(PKT_PLAYER_SOUND);
            write_uuid(&mut out, *channel_id);
            write_uuid(&mut out, *sender);
            write_byte_array(&mut out, opus);
            out.extend_from_slice(&seq.to_be_bytes());
            out.extend_from_slice(&distance.to_be_bytes());
            let mut flags = 0u8;
            if *whispering {
                flags |= SOUND_WHISPER_MASK;
            }
            if category.is_some() {
                flags |= SOUND_HAS_CATEGORY_MASK;
            }
            out.push(flags);
            if let Some(cat) = category {
                write_utf(&mut out, cat);
            }
        }
        UdpPacket::GroupSound {
            channel_id,
            sender,
            opus,
            seq,
            category,
        } => {
            out.push(PKT_GROUP_SOUND);
            write_uuid(&mut out, *channel_id);
            write_uuid(&mut out, *sender);
            write_byte_array(&mut out, opus);
            out.extend_from_slice(&seq.to_be_bytes());
            let flags = if category.is_some() {
                SOUND_HAS_CATEGORY_MASK
            } else {
                0
            };
            out.push(flags);
            if let Some(cat) = category {
                write_utf(&mut out, cat);
            }
        }
        UdpPacket::LocationSound {
            channel_id,
            sender,
            location,
            opus,
            seq,
            distance,
            category,
        } => {
            out.push(PKT_LOCATION_SOUND);
            write_uuid(&mut out, *channel_id);
            write_uuid(&mut out, *sender);
            out.extend_from_slice(&location[0].to_be_bytes());
            out.extend_from_slice(&location[1].to_be_bytes());
            out.extend_from_slice(&location[2].to_be_bytes());
            write_byte_array(&mut out, opus);
            out.extend_from_slice(&seq.to_be_bytes());
            out.extend_from_slice(&distance.to_be_bytes());
            let flags = if category.is_some() {
                SOUND_HAS_CATEGORY_MASK
            } else {
                0
            };
            out.push(flags);
            if let Some(cat) = category {
                write_utf(&mut out, cat);
            }
        }
    }
    out
}

/// Decodes a decrypted inner UDP packet.
pub fn decode_udp_inner(version: SvcVersion, data: &[u8]) -> Option<UdpPacket> {
    let mut r = Reader::new(data);
    let kind = r.u8()?;
    match kind {
        PKT_MIC => Some(UdpPacket::Mic {
            opus: r.byte_array(MAX_OPUS_PAYLOAD)?,
            seq: r.i64()?,
            whispering: r.bool()?,
        }),
        PKT_PLAYER_SOUND => {
            let channel_id = r.uuid()?;
            let sender = r.uuid()?;
            let opus = r.byte_array(MAX_OPUS_PAYLOAD)?;
            let seq = r.i64()?;
            let distance = r.f32()?;
            let flags = r.u8()?;
            let whispering = flags & SOUND_WHISPER_MASK != 0;
            let category = if flags & SOUND_HAS_CATEGORY_MASK != 0 {
                Some(r.utf(16)?)
            } else {
                None
            };
            Some(UdpPacket::PlayerSound {
                channel_id,
                sender,
                opus,
                seq,
                distance,
                whispering,
                category,
            })
        }
        PKT_GROUP_SOUND => {
            let channel_id = r.uuid()?;
            let sender = r.uuid()?;
            let opus = r.byte_array(MAX_OPUS_PAYLOAD)?;
            let seq = r.i64()?;
            let flags = r.u8()?;
            let category = if flags & SOUND_HAS_CATEGORY_MASK != 0 {
                Some(r.utf(16)?)
            } else {
                None
            };
            Some(UdpPacket::GroupSound {
                channel_id,
                sender,
                opus,
                seq,
                category,
            })
        }
        PKT_LOCATION_SOUND => {
            let channel_id = r.uuid()?;
            let sender = r.uuid()?;
            let location = [r.f64()?, r.f64()?, r.f64()?];
            let opus = r.byte_array(MAX_OPUS_PAYLOAD)?;
            let seq = r.i64()?;
            let distance = r.f32()?;
            let flags = r.u8()?;
            let category = if flags & SOUND_HAS_CATEGORY_MASK != 0 {
                Some(r.utf(16)?)
            } else {
                None
            };
            Some(UdpPacket::LocationSound {
                channel_id,
                sender,
                location,
                opus,
                seq,
                distance,
                category,
            })
        }
        PKT_AUTHENTICATE => {
            if version == SvcVersion::V21 {
                Some(UdpPacket::Authenticate {
                    player_uuid: 0,
                    secret: SvcSecret([0; SECRET_BYTES]),
                })
            } else {
                let player_uuid = r.uuid()?;
                let secret: [u8; SECRET_BYTES] = r.take(SECRET_BYTES)?.try_into().ok()?;
                Some(UdpPacket::Authenticate {
                    player_uuid,
                    secret: SvcSecret(secret),
                })
            }
        }
        PKT_AUTHENTICATE_ACK => {
            let nonce = if version == SvcVersion::V21 && r.remaining() >= NONCE_BYTES {
                Some(r.take(NONCE_BYTES)?.try_into().ok()?)
            } else {
                None
            };
            Some(UdpPacket::AuthenticateAck { nonce })
        }
        PKT_PING => Some(UdpPacket::Ping {
            id: r.uuid()?,
            timestamp: r.i64()?,
        }),
        PKT_KEEP_ALIVE => Some(UdpPacket::KeepAlive),
        PKT_CONNECTION_CHECK => {
            let nonce = if version == SvcVersion::V21 && r.remaining() >= NONCE_BYTES {
                Some(r.take(NONCE_BYTES)?.try_into().ok()?)
            } else {
                None
            };
            Some(UdpPacket::ConnectionCheck { nonce })
        }
        PKT_CONNECTION_CHECK_ACK => Some(UdpPacket::ConnectionCheckAck),
        _ => None,
    }
}

/// Wraps and encrypts a client-to-server UDP datagram (`0xFF` + routing UUID + VarInt encrypted bytes).
pub fn encode_client_udp(
    version: SvcVersion,
    routing_id: u128,
    secret: &SvcSecret,
    packet: &UdpPacket,
) -> Result<Vec<u8>, String> {
    let inner = encode_udp_inner(version, packet);
    let encrypted = secret.encrypt(version, &inner)?;
    let mut out = Vec::with_capacity(1 + 16 + 5 + encrypted.len());
    out.push(MAGIC_BYTE);
    write_uuid(&mut out, routing_id);
    write_byte_array(&mut out, &encrypted);
    Ok(out)
}

/// Unwraps and decrypts a server-to-client UDP datagram (`0xFF` + VarInt encrypted bytes).
pub fn decode_server_udp(
    version: SvcVersion,
    secret: &SvcSecret,
    datagram: &[u8],
) -> Option<UdpPacket> {
    let mut r = Reader::new(datagram);
    if r.u8()? != MAGIC_BYTE {
        return None;
    }
    let encrypted = r.byte_array(MAX_UDP_PAYLOAD)?;
    let plain = secret.decrypt(version, &encrypted).ok()?;
    decode_udp_inner(version, &plain)
}

/// Wraps and encrypts a server-to-client UDP datagram (for loopback tests).
pub fn encode_server_udp(
    version: SvcVersion,
    secret: &SvcSecret,
    packet: &UdpPacket,
) -> Result<Vec<u8>, String> {
    let inner = encode_udp_inner(version, packet);
    let encrypted = secret.encrypt(version, &inner)?;
    let mut out = Vec::with_capacity(1 + 5 + encrypted.len());
    out.push(MAGIC_BYTE);
    write_byte_array(&mut out, &encrypted);
    Ok(out)
}

/// Resolves the target UDP `(host, port)` from `SecretPacket.voice_host`, the Minecraft server
/// host/IP, and `SecretPacket.server_port`, matching `InitializationData.parseAddress`.
pub fn parse_voice_host(voice_host: &str, server_ip: &str, server_port: u16) -> (String, u16) {
    // Strip any `:port` from the Minecraft server address if present.
    let base_ip = strip_mc_port(server_ip);
    let trimmed = voice_host.trim();
    if trimmed.is_empty() {
        return (base_ip, server_port);
    }
    // Port-only override (e.g. "24455").
    if let Ok(port) = trimmed.parse::<u16>()
        && port > 0
    {
        return (base_ip, port);
    }
    // Bracketed IPv6: "[::1]" or "[::1]:24455"
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some((host, after)) = rest.split_once(']')
    {
        let port = after
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok())
            .filter(|&p| p > 0)
            .unwrap_or(server_port);
        return (host.to_owned(), port);
    }
    // Single colon: "host:port"
    if let Some((host, port_str)) = trimmed.rsplit_once(':')
        && !host.contains(':')
        && let Ok(port) = port_str.parse::<u16>()
        && port > 0
    {
        let h = if host.is_empty() {
            base_ip
        } else {
            host.to_owned()
        };
        return (h, port);
    }
    // Host-only override (IPv4, hostname, or unbracketed IPv6).
    (trimmed.to_owned(), server_port)
}

fn strip_mc_port(addr: &str) -> String {
    let trimmed = addr.trim();
    if let Some(rest) = trimmed.strip_prefix('[')
        && let Some((host, _)) = rest.split_once(']')
    {
        return host.to_owned();
    }
    if let Some((host, port)) = trimmed.rsplit_once(':')
        && !host.contains(':')
        && port.parse::<u16>().is_ok()
    {
        return host.to_owned();
    }
    trimmed.to_owned()
}

/// Computes proximity attenuation (`0.0..=1.0`) and stereo pan (`-1.0` left .. `+1.0` right)
/// from listener position & Minecraft yaw (`0` = south `+Z`, `90` = west `-X`, `180` = north `-Z`,
/// `270` = east `+X`) to a sound source at `source` with max audible range `max_distance`.
pub fn spatial_params(
    listener: [f64; 3],
    listener_yaw_deg: f32,
    source: [f64; 3],
    max_distance: f32,
) -> (f32, f32) {
    let dx = source[0] - listener[0];
    let dy = source[1] - listener[1];
    let dz = source[2] - listener[2];
    let dist = (dx * dx + dy * dy + dz * dz).sqrt() as f32;
    let max_d = max_distance.max(1.0);
    if dist >= max_d {
        return (0.0, 0.0);
    }
    // Smooth linear-to-gentle rolloff matching OpenAL linear distance clamped model used by SVC.
    let atten = (1.0 - dist / max_d).clamp(0.0, 1.0);
    let horiz = (dx * dx + dz * dz).sqrt();
    if horiz < 0.25 {
        return (atten, 0.0);
    }
    // Minecraft right vector: forward is (-sin(yaw), cos(yaw)), so right is (-cos(yaw), -sin(yaw)).
    let rad = f64::from(listener_yaw_deg).to_radians();
    let (sin_y, cos_y) = rad.sin_cos();
    let right_x = -cos_y;
    let right_z = -sin_y;
    let pan = ((dx * right_x + dz * right_z) / horiz).clamp(-1.0, 1.0) as f32;
    // Soften pan when speaker is right next to the listener so close voices don't jump ears abruptly.
    let close_scale = ((horiz as f32) / 1.5).clamp(0.0, 1.0);
    (atten, pan * close_scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_encrypt_decrypt_roundtrip_all_versions() {
        let secret = SvcSecret(*b"0123456789abcdef");
        let wrong = SvcSecret(*b"fedcba9876543210");
        let msg = b"hello simple voice chat opus frame";
        for version in SvcVersion::PROBE_ORDER {
            let ct = secret.encrypt(version, msg).unwrap();
            assert_eq!(secret.decrypt(version, &ct).unwrap(), msg);
            assert!(wrong.decrypt(version, &ct).is_err());
        }
    }

    #[test]
    fn secret_packet_roundtrip_v21_and_v20() {
        let secret = [7u8; 16];
        let conn_id = 0x11223344_55667788_99aabbcc_ddeeff00u128;
        let player_uuid = 0xaabbccdd_eeff0011_22334455_66778899u128;

        // v21 includes connection_id
        let mut buf21 = Vec::new();
        buf21.extend_from_slice(&secret);
        write_uuid(&mut buf21, conn_id);
        buf21.extend_from_slice(&24454i32.to_be_bytes());
        write_uuid(&mut buf21, player_uuid);
        buf21.push(1); // Audio
        buf21.extend_from_slice(&1024i32.to_be_bytes());
        buf21.extend_from_slice(&48.0f64.to_be_bytes());
        buf21.extend_from_slice(&1000i32.to_be_bytes());
        buf21.push(1); // groupsEnabled
        write_utf(&mut buf21, "voice.example.com:24455");
        buf21.push(0); // allowRecording

        let info21 = decode_secret(SvcVersion::V21, &buf21).unwrap();
        assert_eq!(info21.connection_id, conn_id);
        assert_eq!(info21.player_uuid, player_uuid);
        assert_eq!(info21.server_port, 24454);
        assert_eq!(info21.codec, SvcCodec::Audio);
        assert_eq!(info21.voice_distance, 48.0);
        assert!(info21.groups_enabled);
        assert_eq!(info21.voice_host, "voice.example.com:24455");

        // v20 omits connection_id (falls back to player_uuid)
        let mut buf20 = Vec::new();
        buf20.extend_from_slice(&secret);
        buf20.extend_from_slice(&24454i32.to_be_bytes());
        write_uuid(&mut buf20, player_uuid);
        buf20.push(0); // Voip
        buf20.extend_from_slice(&512i32.to_be_bytes());
        buf20.extend_from_slice(&32.0f64.to_be_bytes());
        buf20.extend_from_slice(&1000i32.to_be_bytes());
        buf20.push(0);
        write_utf(&mut buf20, "");
        buf20.push(1);

        let info20 = decode_secret(SvcVersion::V20, &buf20).unwrap();
        assert_eq!(info20.connection_id, player_uuid);
        assert_eq!(info20.codec, SvcCodec::Voip);
        assert!(!info20.groups_enabled);
    }

    #[test]
    fn udp_packets_roundtrip_encrypted() {
        let secret = SvcSecret([0x42; 16]);
        let uuid = 0x01020304_05060708_090a0b0c_0d0e0f10u128;
        let packets = [
            UdpPacket::Mic {
                opus: vec![1, 2, 3, 4],
                seq: 42,
                whispering: true,
            },
            UdpPacket::PlayerSound {
                channel_id: uuid,
                sender: uuid + 1,
                opus: vec![9, 8, 7],
                seq: 99,
                distance: 48.0,
                whispering: false,
                category: Some("players".into()),
            },
            UdpPacket::GroupSound {
                channel_id: uuid,
                sender: uuid + 2,
                opus: vec![5, 6],
                seq: 100,
                category: None,
            },
            UdpPacket::LocationSound {
                channel_id: uuid,
                sender: uuid + 3,
                location: [10.5, 64.0, -22.25],
                opus: vec![7, 7, 7],
                seq: 101,
                distance: 24.0,
                category: Some("music".into()),
            },
            UdpPacket::KeepAlive,
            UdpPacket::Ping {
                id: uuid,
                timestamp: 123456789,
            },
        ];
        for version in SvcVersion::PROBE_ORDER {
            for pkt in &packets {
                let dgram = encode_server_udp(version, &secret, pkt).unwrap();
                let decoded = decode_server_udp(version, &secret, &dgram).unwrap();
                assert_eq!(&decoded, pkt);
            }
        }
    }

    #[test]
    fn parse_voice_host_cases() {
        assert_eq!(
            parse_voice_host("", "play.example.com:25565", 24454),
            ("play.example.com".into(), 24454)
        );
        assert_eq!(
            parse_voice_host("24455", "192.168.1.5:25565", 24454),
            ("192.168.1.5".into(), 24455)
        );
        assert_eq!(
            parse_voice_host("voice.example.com:25000", "play.example.com", 24454),
            ("voice.example.com".into(), 25000)
        );
        assert_eq!(
            parse_voice_host("voice.example.com", "play.example.com", 24454),
            ("voice.example.com".into(), 24454)
        );
        assert_eq!(
            parse_voice_host("[::1]:24456", "127.0.0.1:25565", 24454),
            ("::1".into(), 24456)
        );
    }

    #[test]
    fn spatial_pan_and_attenuation() {
        // Facing south (+Z, yaw=0): east (+X) is left (-1), west (-X) is right (+1).
        let (atten_r, pan_r) = spatial_params([0.0, 64.0, 0.0], 0.0, [-10.0, 64.0, 0.0], 48.0);
        assert!(atten_r > 0.75 && atten_r < 0.85);
        assert!((pan_r - 1.0).abs() < 0.01);

        let (atten_l, pan_l) = spatial_params([0.0, 64.0, 0.0], 0.0, [10.0, 64.0, 0.0], 48.0);
        assert!((atten_l - atten_r).abs() < 0.01);
        assert!((pan_l + 1.0).abs() < 0.01);

        // Beyond max distance -> silent.
        let (atten_far, _) = spatial_params([0.0, 64.0, 0.0], 0.0, [60.0, 64.0, 0.0], 48.0);
        assert_eq!(atten_far, 0.0);
    }
}
