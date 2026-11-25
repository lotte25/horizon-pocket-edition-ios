// Copyright (c) 2026 Ludovic Marie and the Horizon contributors.
// SPDX-License-Identifier: MIT
//! Generic authlib-injector (Yggdrasil) authentication: Drasl, Blessing Skin, LittleSkin, and any
//! custom Yggdrasil server. Passwords are used for one request only; the existing `SecretStore`
//! holds the access token and client token.
use super::*;

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .redirects(5)
        .user_agent(&format!("starling/{}", env!("CARGO_PKG_VERSION")))
        .build()
}

pub(super) const REFUSED: &str = "starling.account.error.authlib_refused";

/// Server metadata resolved from the Yggdrasil API root.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerMeta {
    pub api_root: String,
    pub server_name: Option<String>,
    pub skin_domains: Vec<String>,
}

/// Normalize a user-supplied server address into an `https://` (or explicit `http://`) base URL
/// without trailing slashes.
pub fn normalize_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("starling.account.error.authlib_url".into());
    }
    let with_scheme = if trimmed.starts_with("https://") || trimmed.starts_with("http://") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    };
    let rest = with_scheme
        .strip_prefix("https://")
        .or_else(|| with_scheme.strip_prefix("http://"))
        .unwrap_or("");
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() || host.chars().any(char::is_whitespace) {
        return Err("starling.account.error.authlib_url".into());
    }
    Ok(with_scheme.trim_end_matches('/').to_owned())
}

/// Extract the host (without port) from a normalized URL, for skin domain fallback and UI labels.
pub fn url_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split(']').next()?
    } else {
        authority.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Resolve an authlib-injector API Location (ALI) relative or absolute header value against `base`.
fn resolve_ali(base: &str, location: &str) -> Result<String, String> {
    let loc = location.trim();
    if loc.starts_with("https://") || loc.starts_with("http://") {
        return normalize_url(loc);
    }
    let scheme_end = base
        .find("://")
        .map(|i| i + 3)
        .ok_or_else(|| "starling.account.error.authlib_url".to_owned())?;
    let origin = match base[scheme_end..].find('/') {
        Some(slash) => &base[..scheme_end + slash],
        None => base,
    };
    let resolved = if loc.starts_with('/') {
        format!("{origin}{loc}")
    } else {
        format!("{}/{}", base.trim_end_matches('/'), loc)
    };
    normalize_url(&resolved)
}

/// Discover the Yggdrasil API root following authlib-injector's ALI protocol
/// (`X-Authlib-Injector-API-Location` header) and parse `skinDomains` & `meta.serverName`.
pub fn discover(raw_url: &str) -> Result<ServerMeta, String> {
    let initial = normalize_url(raw_url)?;
    discover_with(&initial)
}

fn discover_with(initial: &str) -> Result<ServerMeta, String> {
    let mut current = initial.to_owned();
    let mut body_value: Option<Value> = None;
    for _ in 0..2 {
        match agent().get(&current).call() {
            Ok(resp) => {
                if let Some(ali) = resp.header("X-Authlib-Injector-API-Location")
                    && !ali.trim().is_empty()
                {
                    let next = resolve_ali(&current, ali)?;
                    if next != current {
                        current = next;
                        continue;
                    }
                }
                if let Ok(text) = resp.into_string()
                    && let Ok(val) = serde_json::from_str::<Value>(&text)
                {
                    body_value = Some(val);
                }
                break;
            }
            Err(ureq::Error::Status(_, resp)) => {
                if let Some(ali) = resp.header("X-Authlib-Injector-API-Location")
                    && !ali.trim().is_empty()
                {
                    let next = resolve_ali(&current, ali)?;
                    if next != current {
                        current = next;
                        continue;
                    }
                }
                break;
            }
            Err(ureq::Error::Transport(t)) => {
                return Err(format!(
                    "{UNREACHABLE}{current} ({}). Check the server URL and your connection.",
                    transport_detail(&t)
                ));
            }
        }
    }

    let mut skin_domains = Vec::new();
    let mut server_name = None;
    if let Some(val) = body_value {
        if let Some(arr) = val.get("skinDomains").and_then(Value::as_array) {
            for item in arr {
                if let Some(d) = item.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    skin_domains.push(d.to_ascii_lowercase());
                }
            }
        }
        server_name = val
            .get("meta")
            .and_then(|m| m.get("serverName"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
    }
    if let Some(host) = url_host(&current)
        && !skin_domains.iter().any(|d| d == &host)
    {
        skin_domains.push(host);
    }
    Ok(ServerMeta {
        api_root: current,
        server_name,
        skin_domains,
    })
}

fn exchange(endpoint: &str, body: Value) -> Result<Value, String> {
    let (status, value) = send(
        agent().post(endpoint),
        Some(Body::Json(body)),
        "authlib-injector",
    )?;
    match status {
        200 => Ok(value),
        400 | 401 | 403 => {
            if let Some(msg) = value
                .get("errorMessage")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                Err(msg.to_owned())
            } else {
                Err(REFUSED.into())
            }
        }
        429 => Err("starling.account.error.authlib_limit".into()),
        _ => Err("starling.account.error.authlib_unavailable".into()),
    }
}

fn decode(
    value: &Value,
    client: &str,
    api_root: &str,
    skin_domains: Vec<String>,
) -> Result<(Account, Secrets), String> {
    let profile = value
        .get("selectedProfile")
        .filter(|v| v.is_object())
        .or_else(|| {
            value
                .get("availableProfiles")
                .and_then(Value::as_array)
                .and_then(|arr| arr.first())
        })
        .ok_or("starling.account.error.authlib_profile")?;
    let uuid = str_field(profile, "id")
        .unwrap_or_default()
        .replace('-', "")
        .to_lowercase();
    let name = str_field(profile, "name").unwrap_or_default();
    if uuid.len() != 32
        || !uuid.bytes().all(|b| b.is_ascii_hexdigit())
        || !(1..=16).contains(&name.len())
    {
        return Err("starling.account.error.authlib_profile".into());
    }
    let token = str_field(value, "accessToken")
        .filter(|t| !t.is_empty())
        .ok_or("starling.account.error.authlib_session")?;
    let returned_client = str_field(value, "clientToken").unwrap_or_else(|| client.to_owned());
    Ok((
        Account {
            provider: Provider::AuthlibInjector,
            uuid,
            name,
            skin: None,
            cape: None,
            api_url: Some(api_root.to_owned()),
            skin_domains,
        },
        Secrets {
            provider: Provider::AuthlibInjector,
            refresh_token: returned_client,
            minecraft_token: token,
            minecraft_expires_at: now() + 3600,
            xuid: None,
        },
    ))
}

/// Authenticate against a custom authlib-injector / Yggdrasil server.
/// Neither password nor raw credentials are persisted or logged.
pub fn authenticate(
    raw_url: &str,
    username: &str,
    password: &str,
) -> Result<(Account, Secrets), String> {
    if username.trim().is_empty() || password.is_empty() {
        return Err("starling.account.error.authlib_credentials".into());
    }
    let meta = discover(raw_url)?;
    let (mut account, secrets) = authenticate_with(&meta, username, password)?;
    update_textures(&mut account);
    Ok((account, secrets))
}

fn authenticate_with(
    meta: &ServerMeta,
    username: &str,
    password: &str,
) -> Result<(Account, Secrets), String> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|_| "starling.account.error.authlib_random")?;
    let client: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let value = exchange(
        &format!("{}/authserver/authenticate", meta.api_root),
        json!({
            "agent": { "name": "Minecraft", "version": 1 },
            "username": username.trim(),
            "password": password,
            "clientToken": client,
            "requestUser": true,
        }),
    )?;
    decode(&value, &client, &meta.api_root, meta.skin_domains.clone())
}

pub(super) fn refresh(account: &Account, secrets: &Secrets) -> Result<(Account, Secrets), String> {
    let api_root = account
        .api_url
        .as_deref()
        .ok_or("starling.account.error.authlib_url")?;
    let (mut renewed, secrets) = refresh_with(api_root, account, secrets)?;
    update_textures(&mut renewed);
    Ok((renewed, secrets))
}

fn refresh_with(
    api_root: &str,
    account: &Account,
    secrets: &Secrets,
) -> Result<(Account, Secrets), String> {
    // Check if the current token is still valid first (some Yggdrasil servers prefer validate).
    let validate_ok = send(
        agent().post(&format!("{api_root}/authserver/validate")),
        Some(Body::Json(json!({
            "accessToken": secrets.minecraft_token,
            "clientToken": secrets.refresh_token,
        }))),
        "authlib-injector",
    )
    .is_ok_and(|(status, _)| status == 204 || status == 200);
    if validate_ok {
        let mut renewed_secrets = secrets.clone();
        renewed_secrets.minecraft_expires_at = now() + 3600;
        return Ok((account.clone(), renewed_secrets));
    }

    let value = exchange(
        &format!("{api_root}/authserver/refresh"),
        json!({
            "accessToken": secrets.minecraft_token,
            "clientToken": secrets.refresh_token,
            "requestUser": true,
        }),
    )?;
    let (renewed, new_secrets) = decode(
        &value,
        &secrets.refresh_token,
        api_root,
        account.skin_domains.clone(),
    )?;
    if renewed.uuid != account.uuid {
        return Err("starling.account.error.authlib_changed".into());
    }
    Ok((renewed, new_secrets))
}

pub(super) fn fresh(account: &Account, secrets: Secrets) -> Result<(Account, Secrets), String> {
    if secrets.minecraft_expires_at > now() + 1200 {
        return Ok((account.clone(), secrets));
    }
    refresh(account, &secrets)
}

/// Fetch the player's own skin & cape from `<api_root>/sessionserver/session/minecraft/profile/<uuid>`.
fn update_textures(account: &mut Account) {
    let Some(api_root) = account.api_url.as_deref() else {
        return;
    };
    let url = format!(
        "{api_root}/sessionserver/session/minecraft/profile/{}",
        account.uuid
    );
    let Ok((200, value)) = send(agent().get(&url), None, "authlib-injector profile") else {
        return;
    };
    let Some(props) = value.get("properties").and_then(Value::as_array) else {
        return;
    };
    let Some(b64) = props
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == Some("textures"))
        .and_then(|p| p.get("value"))
        .and_then(Value::as_str)
    else {
        return;
    };
    let Some(decoded) = decode_base64(b64) else {
        return;
    };
    let Ok(tex_doc) = serde_json::from_slice::<Value>(&decoded) else {
        return;
    };
    let textures = &tex_doc["textures"];
    if let Some(skin_url) = str_field(&textures["SKIN"], "url") {
        let model = if textures["SKIN"]["metadata"]["model"].as_str() == Some("slim") {
            "slim"
        } else {
            "classic"
        };
        account.skin = Some((skin_url, model.into()));
    }
    if let Some(cape_url) = str_field(&textures["CAPE"], "url") {
        account.cape = Some(cape_url);
    }
}

/// Minimal standard Base64 decoder (RFC 4648) so `starling-account` needs no extra crate dependency.
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for byte in input.bytes() {
        let val = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return None,
        };
        buf = (buf << 6) | u32::from(val);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Stable 64-bit FNV-1a hash of the API URL for purely alphanumeric account keys (`SealedStore`).
pub(super) fn url_key_hash(api_url: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in api_url.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn normalize_and_resolve_ali_urls() {
        assert_eq!(
            normalize_url("drasl.moebytes.org/authlib-injector/").unwrap(),
            "https://drasl.moebytes.org/authlib-injector"
        );
        assert_eq!(
            resolve_ali("https://drasl.moebytes.org", "/authlib-injector/").unwrap(),
            "https://drasl.moebytes.org/authlib-injector"
        );
        assert_eq!(
            resolve_ali("https://example.com/sub", "api/yggdrasil").unwrap(),
            "https://example.com/sub/api/yggdrasil"
        );
        assert_eq!(
            url_host("https://drasl.moebytes.org:8443/authlib-injector").as_deref(),
            Some("drasl.moebytes.org")
        );
        assert!(normalize_url("   ").is_err());
    }

    #[test]
    fn discover_authenticate_and_refresh_against_mock_yggdrasil() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_bg = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten().take(5) {
                let mut s = stream;
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let line = req.lines().next().unwrap_or("");
                let path = line.split_whitespace().nth(1).unwrap_or("");
                seen_bg.lock().unwrap().push(line.to_owned());
                let (status, headers, body) = match path {
                    "/" => (
                        "200 OK",
                        "X-Authlib-Injector-API-Location: /authlib-injector\r\n",
                        "{}".to_owned(),
                    ),
                    "/authlib-injector" => (
                        "200 OK",
                        "",
                        json!({
                            "meta": {"serverName": "Drasl Test"},
                            "skinDomains": ["drasl.example", ".cdn.example"]
                        })
                        .to_string(),
                    ),
                    "/authlib-injector/authserver/authenticate" => (
                        "200 OK",
                        "",
                        json!({
                            "accessToken": "tok-1",
                            "clientToken": "cli-1",
                            "selectedProfile": {
                                "id": "0123456789abcdef0123456789abcdef",
                                "name": "Tester"
                            }
                        })
                        .to_string(),
                    ),
                    "/authlib-injector/authserver/validate" => ("403 Forbidden", "", "{}".into()),
                    "/authlib-injector/authserver/refresh" => (
                        "200 OK",
                        "",
                        json!({
                            "accessToken": "tok-2",
                            "clientToken": "cli-1",
                            "selectedProfile": {
                                "id": "0123456789abcdef0123456789abcdef",
                                "name": "Tester"
                            }
                        })
                        .to_string(),
                    ),
                    _ => ("404 Not Found", "", "{}".into()),
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });

        let base = format!("http://{addr}");
        let meta = discover_with(&base).unwrap();
        assert_eq!(meta.api_root, format!("{base}/authlib-injector"));
        assert_eq!(meta.server_name.as_deref(), Some("Drasl Test"));
        assert!(meta.skin_domains.iter().any(|d| d == "drasl.example"));

        let (account, secrets) = authenticate_with(&meta, "user@example", "pw").unwrap();
        assert_eq!(account.provider, Provider::AuthlibInjector);
        assert_eq!(account.name, "Tester");
        assert!(account.key().bytes().all(|b| b.is_ascii_alphanumeric()));
        assert!(account.key().starts_with("ali"));

        let (renewed, renewed_secrets) = refresh_with(&meta.api_root, &account, &secrets).unwrap();
        assert_eq!(renewed.uuid, account.uuid);
        assert_eq!(renewed_secrets.access_token(), "tok-2");
    }
}
