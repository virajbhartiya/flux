//! Two-factor authentication: TOTP (RFC 6238, 30 s, 6 digits, SHA-1 — what
//! every authenticator app supports) with one-time recovery codes.
//!
//! Secrets are sealed with the server key when one is configured. Each code
//! works once (the last accepted time step is remembered), and a step of
//! clock skew either way is tolerated. Sign-ins with a password or an
//! external provider ask for a code before a session is issued.

use hmac::Mac as _;
use sha2::Digest as _;

use crate::auth::{Principal, now_ms};
use crate::core::{Flux, FluxError, FluxResult};

const STEP_S: i64 = 30;
const DIGITS: u32 = 6;
const RECOVERY_CODES: usize = 10;

pub fn base32(data: &[u8]) -> String {
    const A: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(A[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(A[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let (mut buf, mut bits, mut out) = (0u32, 0u32, Vec::new());
    for c in s.chars().filter(|c| !c.is_whitespace() && *c != '=' && *c != '-') {
        let v = match c.to_ascii_uppercase() {
            c @ 'A'..='Z' => c as u32 - 'A' as u32,
            c @ '2'..='7' => c as u32 - '2' as u32 + 26,
            _ => return None,
        };
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push((buf >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

/// The code for a time step.
pub fn totp_at(secret: &[u8], step: i64) -> String {
    let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(secret).expect("any key length");
    mac.update(&(step as u64).to_be_bytes());
    let h = mac.finalize().into_bytes();
    let o = (h[19] & 0x0f) as usize;
    let bin = u32::from_be_bytes([h[o] & 0x7f, h[o + 1], h[o + 2], h[o + 3]]);
    format!("{:0width$}", bin % 10u32.pow(DIGITS), width = DIGITS as usize)
}

/// The time step a code matches (current ±1), if any.
pub fn verify(secret: &[u8], code: &str, now_s: i64) -> Option<i64> {
    let code: String = code.chars().filter(char::is_ascii_digit).collect();
    if code.len() != DIGITS as usize {
        return None;
    }
    let step = now_s / STEP_S;
    (step - 1..=step + 1).find(|s| constant_eq(totp_at(secret, *s).as_bytes(), code.as_bytes()))
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hash_code(code: &str) -> String {
    let norm: String = code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    hex::encode(sha2::Sha256::digest(norm.as_bytes()))
}

/// `otpauth://` URI for authenticator apps (and QR codes).
pub fn otpauth_uri(issuer: &str, account: &str, secret_b32: &str) -> String {
    let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    format!(
        "otpauth://totp/{}:{}?secret={secret_b32}&issuer={}&algorithm=SHA1&digits=6&period=30",
        enc(issuer),
        enc(account),
        enc(issuer)
    )
}

/// A QR code as inline SVG markup.
pub fn qr_svg(data: &str) -> String {
    match qrcode::QrCode::new(data.as_bytes()) {
        Ok(code) => code
            .render::<qrcode::render::svg::Color<'_>>()
            .min_dimensions(200, 200)
            .quiet_zone(true)
            .dark_color(qrcode::render::svg::Color("#111"))
            .light_color(qrcode::render::svg::Color("#fff"))
            .build(),
        Err(_) => String::new(),
    }
}

/// A pending enrollment: the secret the user is setting up.
#[derive(Debug, Clone)]
pub struct Enrollment {
    pub secret_b32: String,
    pub uri: String,
}

impl Flux {
    fn seal_totp(&self, user_id: i64, secret_b32: &str) -> FluxResult<String> {
        if self.settings.secret_key.is_none() {
            return Ok(secret_b32.to_string());
        }
        let sealed = self.seal(format!("flux-totp/{user_id}").as_bytes(), secret_b32.as_bytes())?;
        Ok(format!("enc:{}", hex::encode(sealed)))
    }

    fn unseal_totp(&self, user_id: i64, stored: &str) -> FluxResult<Vec<u8>> {
        let b32 = match stored.strip_prefix("enc:") {
            Some(h) => {
                let raw = hex::decode(h).map_err(|_| FluxError::Internal(anyhow::anyhow!("corrupt 2FA secret")))?;
                String::from_utf8(self.open_sealed(format!("flux-totp/{user_id}").as_bytes(), &raw)?)
                    .map_err(|_| FluxError::Internal(anyhow::anyhow!("corrupt 2FA secret")))?
            }
            None => stored.to_string(),
        };
        base32_decode(&b32).ok_or_else(|| FluxError::Internal(anyhow::anyhow!("corrupt 2FA secret")))
    }

    pub async fn two_factor_enabled(&self, user_id: i64) -> FluxResult<bool> {
        Ok(self.meta.totp(user_id).await?.0.is_some())
    }

    /// Start enrolling: a fresh secret, kept until confirmed.
    pub async fn begin_totp(&self, who: &Principal) -> FluxResult<Enrollment> {
        let user = who.user.as_ref().ok_or_else(|| FluxError::Unauthenticated("sign in first".into()))?;
        let mut raw = [0u8; 20];
        getrandom::fill(&mut raw).expect("OS random number generator");
        let secret_b32 = base32(&raw);
        let issuer = self
            .settings
            .public_url
            .as_deref()
            .and_then(|u| url::Url::parse(u).ok()?.host_str().map(str::to_string))
            .unwrap_or_else(|| "flux".into());
        let e = Enrollment { uri: otpauth_uri(&issuer, &user.name, &secret_b32), secret_b32 };
        self.pending_totp.insert(user.id, e.clone());
        Ok(e)
    }

    pub fn pending_totp(&self, user_id: i64) -> Option<Enrollment> {
        self.pending_totp.get(&user_id)
    }

    /// Confirm enrollment with a code; returns the recovery codes (shown once).
    pub async fn enable_totp(&self, who: &Principal, code: &str) -> FluxResult<Vec<String>> {
        let user = who.user.as_ref().ok_or_else(|| FluxError::Unauthenticated("sign in first".into()))?;
        let e = self.pending_totp.get(&user.id).ok_or_else(|| FluxError::FailedPrecondition("start the setup again".into()))?;
        let secret = base32_decode(&e.secret_b32).expect("we encoded it");
        if verify(&secret, code, now_ms() / 1000).is_none() {
            return Err(FluxError::InvalidArgument("that code is not right; check your device's clock".into()));
        }
        let codes: Vec<String> = (0..RECOVERY_CODES)
            .map(|_| {
                let mut b = [0u8; 5];
                getrandom::fill(&mut b).expect("OS random number generator");
                let h = hex::encode(b);
                format!("{}-{}", &h[..5], &h[5..])
            })
            .collect();
        let hashes: Vec<String> = codes.iter().map(|c| hash_code(c)).collect();
        self.meta.set_totp(user.id, Some(&self.seal_totp(user.id, &e.secret_b32)?), &hashes).await?;
        self.pending_totp.invalidate(&user.id);
        self.audit(who, "user.2fa_enabled", &user.name, "").await;
        Ok(codes)
    }

    pub async fn disable_totp(&self, who: &Principal, code: &str) -> FluxResult<()> {
        let user = who.user.as_ref().ok_or_else(|| FluxError::Unauthenticated("sign in first".into()))?;
        if !self.check_second_factor(user.id, code).await? {
            return Err(FluxError::PermissionDenied("that code is not right".into()));
        }
        self.meta.set_totp(user.id, None, &[]).await?;
        self.audit(who, "user.2fa_disabled", &user.name, "").await;
        Ok(())
    }

    /// Check a TOTP or recovery code (recovery codes are consumed).
    pub async fn check_second_factor(&self, user_id: i64, code: &str) -> FluxResult<bool> {
        let (stored, recovery) = self.meta.totp(user_id).await?;
        let Some(stored) = stored else { return Ok(true) };
        let secret = self.unseal_totp(user_id, &stored)?;
        if let Some(step) = verify(&secret, code, now_ms() / 1000) {
            // Each code works once.
            if self.totp_used.get(&user_id).is_some_and(|last| last >= step) {
                return Ok(false);
            }
            self.totp_used.insert(user_id, step);
            return Ok(true);
        }
        let h = hash_code(code);
        if let Some(i) = recovery.iter().position(|r| constant_eq(r.as_bytes(), h.as_bytes())) {
            let mut rest = recovery.clone();
            rest.remove(i);
            self.meta.set_totp(user_id, Some(&stored), &rest).await?;
            let name = self.meta.get_account_by_id(user_id).await?.map(|a| a.user.name).unwrap_or_default();
            self.meta.audit_append(&name, "user.recovery_code_used", &name, &format!("{} left", rest.len())).await?;
            return Ok(true);
        }
        Ok(false)
    }

    pub async fn recovery_codes_left(&self, user_id: i64) -> FluxResult<usize> {
        Ok(self.meta.totp(user_id).await?.1.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_vectors() {
        // RFC 6238 appendix B (SHA-1 secret "12345678901234567890"), 6 digits.
        let s = b"12345678901234567890";
        assert_eq!(totp_at(s, 59 / 30), "287082");
        assert_eq!(totp_at(s, 1_111_111_109 / 30), "081804");
        assert_eq!(totp_at(s, 1_234_567_890 / 30), "005924");
        assert_eq!(totp_at(s, 2_000_000_000 / 30), "279037");
        assert_eq!(verify(s, "287 082", 59), Some(1));
        assert_eq!(verify(s, "287082", 59 + 30), Some(1), "one step of skew");
        assert_eq!(verify(s, "287082", 59 + 90), None);
    }

    #[test]
    fn base32_round_trip() {
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_decode("MZXW6YTBOI").unwrap(), b"foobar");
        assert_eq!(base32_decode("mzxw 6ytb oi").unwrap(), b"foobar");
        let raw = [7u8; 20];
        assert_eq!(base32_decode(&base32(&raw)).unwrap(), raw);
    }
}
