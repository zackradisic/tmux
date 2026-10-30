//! Who the plugin is on Slack: the desktop app's own session.
//!
//! There is no bot and no OAuth app. The Slack desktop app keeps a
//! session cookie (`d=xoxd-…`) in a Chromium cookie store, encrypted
//! with a key derived from the "Slack Safe Storage" Keychain item; the
//! workspace page loaded with that cookie embeds a live session token
//! (`xoxc-…`). With the two, the plugin is the user: every method the
//! desktop client calls (`client.counts`, `rtm.connect`, the sidebar
//! sections) answers, and a message it posts is the user's.
//!
//! The Keychain password and the cookie database are read by two small
//! jobs (`security`, `sqlite3` on a copy); the PBKDF2 and AES-CBC are
//! done here, so the decrypted secret only ever exists in this plugin
//! and its store. The derivation runs once; afterwards the stored
//! identity is checked with `auth.test` and re-derived only when Slack
//! rejects it (the app signed out, the cookie rotated).

use aes::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use tmux_plugin_sdk::prelude::*;

use crate::http::{self, sq};
use crate::store::{self, Identity};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

/// PBKDF2-HMAC-SHA1, the first 16 bytes (one block covers it).
fn pbkdf2_sha1_16(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 16] {
    let mut mac = Hmac::<Sha1>::new_from_slice(password).expect("hmac takes any key length");
    mac.update(salt);
    mac.update(&1u32.to_be_bytes());
    let mut u = mac.finalize().into_bytes();
    let mut t = u;
    for _ in 1..iterations.max(1) {
        let mut m = Hmac::<Sha1>::new_from_slice(password).expect("hmac takes any key length");
        m.update(&u);
        u = m.finalize().into_bytes();
        for (a, b) in t.iter_mut().zip(u.iter()) {
            *a ^= b;
        }
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&t[..16]);
    out
}

/// Chromium "Safe Storage" cookie decryption: `v10`/`v11` prefix, AES-128-CBC,
/// IV of 16 spaces, PKCS#7; newer builds prepend a 32-byte hash of the
/// host key to the plaintext.
pub fn decrypt_cookie(encrypted: &[u8], password: &[u8], iterations: u32) -> Option<String> {
    if encrypted.len() < 3 + 16 || !(encrypted.starts_with(b"v10") || encrypted.starts_with(b"v11")) {
        return None;
    }
    let key = pbkdf2_sha1_16(password, b"saltysalt", iterations);
    let iv = [b' '; 16];
    let mut buf = encrypted[3..].to_vec();
    if buf.len() % 16 != 0 {
        return None;
    }
    let plain = Aes128CbcDec::new(&key.into(), &iv.into()).decrypt_padded_mut::<NoPadding>(&mut buf).ok()?;
    let mut plain = plain.to_vec();
    if let Some(&pad) = plain.last() {
        if (1..=16).contains(&pad) && plain.len() >= pad as usize {
            plain.truncate(plain.len() - pad as usize);
        }
    }
    if let Ok(s) = std::str::from_utf8(&plain) {
        if s.starts_with("xoxd-") {
            return Some(s.to_string());
        }
    }
    if plain.len() > 32 {
        if let Ok(s) = std::str::from_utf8(&plain[32..]) {
            if s.starts_with("xoxd-") {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Where the desktop app keeps its cookies, and how the store is keyed,
/// per platform.
struct CookieSource {
    db: String,
    /// `None`: a fixed password (Chromium on Linux without a keyring).
    keychain: Option<&'static str>,
    fixed_password: &'static [u8],
    iterations: u32,
}

fn cookie_sources(home: &str) -> Vec<CookieSource> {
    vec![
        CookieSource {
            db: format!("{home}/Library/Application Support/Slack/Cookies"),
            keychain: Some("Slack Safe Storage"),
            fixed_password: b"",
            iterations: 1003,
        },
        CookieSource {
            db: format!("{home}/.config/Slack/Cookies"),
            keychain: None,
            fixed_password: b"peanuts",
            iterations: 1,
        },
    ]
}

/// The `d` cookie of the signed-in desktop app.
pub async fn desktop_cookie() -> Result<String, String> {
    let home = home_dir().map_err(|e| e.message.clone())?;
    let root = fs_root().map_err(|e| e.message.clone())?;
    let mut last = String::from("no Slack desktop cookie store found");
    for src in cookie_sources(&home) {
        let probe = run_job(&format!("test -f {}", sq(&src.db)), None).await.map_err(|e| e.message.clone())?;
        if probe.status != 0 {
            continue;
        }
        let password: Vec<u8> = match src.keychain {
            Some(service) => {
                let out = run_job(&format!("security find-generic-password -ws {}", sq(service)), None)
                    .await
                    .map_err(|e| e.message.clone())?;
                if out.status != 0 {
                    last = format!("Keychain: {}", out.output.trim());
                    continue;
                }
                out.output.trim_end_matches(['\n', '\r']).as_bytes().to_vec()
            }
            None => src.fixed_password.to_vec(),
        };
        // The app holds the database open (and locked); read a copy.
        let copy = format!("{root}/stage/cookies-{}", now_ms());
        let cmd = format!(
            "umask 077; mkdir -p {} && cp {} {} && sqlite3 {} \"select hex(encrypted_value) from cookies where name='d' and host_key like '%slack.com' order by expires_utc desc limit 1\"; rc=$?; rm -f {}; exit $rc",
            sq(&format!("{root}/stage")),
            sq(&src.db),
            sq(&copy),
            sq(&copy),
            sq(&copy)
        );
        let out = run_job(&cmd, None).await.map_err(|e| e.message.clone())?;
        if out.status != 0 {
            last = format!("cookie store: {}", out.output.trim());
            continue;
        }
        let Some(enc) = unhex(out.output.trim()) else {
            last = "cookie store: no `d` cookie for slack.com (is the app signed in?)".into();
            continue;
        };
        match decrypt_cookie(&enc, &password, src.iterations) {
            Some(c) => return Ok(c),
            None => last = "cookie store: the `d` cookie did not decrypt (Keychain password changed?)".into(),
        }
    }
    Err(last)
}

/// Load the workspace page with the cookie and read the session token
/// Slack embeds in it.
pub async fn mint_token(domain: &str, cookie: &str) -> Result<String, String> {
    let (status, html) = http::get_with_cookie(&format!("https://{domain}.slack.com/"), cookie).await?;
    let Some(i) = html.find("\"api_token\":\"") else {
        return Err(format!("{domain}.slack.com: HTTP {status}, no api_token in the page (not signed into this workspace?)"));
    };
    let rest = &html[i + "\"api_token\":\"".len()..];
    let end = rest.find('"').unwrap_or(rest.len());
    let tok = &rest[..end];
    if !tok.starts_with("xox") {
        return Err(format!("{domain}.slack.com: unexpected token shape"));
    }
    Ok(tok.to_string())
}

/// `auth.test` on a candidate identity: fills in who it is.
async fn test(mut id: Identity) -> Result<Identity, String> {
    let v = http::call(&id, "auth.test", &[]).await?;
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    id.user_id = get("user_id");
    id.team_id = get("team_id");
    id.team_name = get("team");
    id.user_name = get("user");
    if id.user_id.is_empty() {
        return Err("auth.test: no user".into());
    }
    Ok(id)
}

/// The identity for `domain`: the stored one when Slack still accepts
/// it, else derived afresh from the desktop app.
pub async fn ensure(domain: &str) -> Result<Identity, String> {
    if let Some(stored) = store::get_auth(domain).await {
        match test(stored.clone()).await {
            Ok(id) => return Ok(id),
            Err(e) => log(&format!("slack: stored session for {domain} rejected ({e}); deriving again")),
        }
    }
    let id = derive(domain).await?;
    store::set_auth(&id).await.map_err(|e| e.message.clone())?;
    Ok(id)
}

pub async fn derive(domain: &str) -> Result<Identity, String> {
    let cookie = desktop_cookie().await?;
    let token = mint_token(domain, &cookie).await?;
    let id = Identity {
        domain: domain.to_string(),
        token,
        cookie,
        ..Default::default()
    };
    test(id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncryptMut;

    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

    fn encrypt(plain: &[u8], password: &[u8], iterations: u32) -> Vec<u8> {
        let key = pbkdf2_sha1_16(password, b"saltysalt", iterations);
        let iv = [b' '; 16];
        let pad = 16 - plain.len() % 16;
        let mut buf = plain.to_vec();
        buf.extend(std::iter::repeat(pad as u8).take(pad));
        let n = buf.len();
        Aes128CbcEnc::new(&key.into(), &iv.into()).encrypt_padded_mut::<NoPadding>(&mut buf, n).unwrap();
        let mut out = b"v10".to_vec();
        out.extend(buf);
        out
    }

    #[test]
    fn pbkdf2_matches_a_known_vector() {
        // RFC 6070: P="password", S="salt", c=2, dkLen=20 -> ea6c014d c72d6f8c cd1ed92a ce1d41f0 d8de8957
        let k = pbkdf2_sha1_16(b"password", b"salt", 2);
        assert_eq!(k.to_vec(), vec![0xea, 0x6c, 0x01, 0x4d, 0xc7, 0x2d, 0x6f, 0x8c, 0xcd, 0x1e, 0xd9, 0x2a, 0xce, 0x1d, 0x41, 0xf0]);
    }

    #[test]
    fn decrypts_a_cookie_round_trip() {
        let enc = encrypt(b"xoxd-abc-def", b"hunter2", 1003);
        assert_eq!(decrypt_cookie(&enc, b"hunter2", 1003).as_deref(), Some("xoxd-abc-def"));
        assert_eq!(decrypt_cookie(&enc, b"wrong", 1003), None);
        // The newer layout: a 32-byte host hash before the value.
        let mut plain = vec![0xabu8; 32];
        plain.extend_from_slice(b"xoxd-newer");
        let enc = encrypt(&plain, b"peanuts", 1);
        assert_eq!(decrypt_cookie(&enc, b"peanuts", 1).as_deref(), Some("xoxd-newer"));
    }

    #[test]
    fn unhex_parses() {
        assert_eq!(unhex("763130"), Some(b"v10".to_vec()));
        assert_eq!(unhex("7"), None);
    }
}
