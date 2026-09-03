//! Client-side AES-256-GCM and a self-contained WebCrypto viewer.

use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{Aead, Payload},
};

use crate::{
    config::Branding,
    digest::pbkdf2_hmac_sha256,
    encoding::{base64, html_escape},
    error::{Error, Result},
    redact::Secret,
};

/// Browser and native key-derivation work factor.
pub const PBKDF2_ITERATIONS: u32 = 210_000;

/// Ciphertext and all non-secret values required to decrypt it.
#[derive(Clone, PartialEq, Eq)]
pub struct EncryptedPayload {
    /// Random PBKDF2 salt.
    pub salt: [u8; 16],
    /// Random AES-GCM nonce.
    pub nonce: [u8; 12],
    /// AES-GCM ciphertext with its authentication tag appended.
    pub ciphertext: Vec<u8>,
    /// Authenticated associated data.
    pub aad: Vec<u8>,
    /// Original media type.
    pub content_type: String,
    /// PBKDF2 work factor.
    pub iterations: u32,
}

impl std::fmt::Debug for EncryptedPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedPayload")
            .field("salt", &self.salt)
            .field("nonce", &self.nonce)
            .field("ciphertext_bytes", &self.ciphertext.len())
            .field("aad", &String::from_utf8_lossy(&self.aad))
            .field("content_type", &self.content_type)
            .field("iterations", &self.iterations)
            .finish()
    }
}

/// Encrypts bytes before they leave the machine.
pub fn encrypt(
    plaintext: &[u8],
    content_type: &str,
    password: &Secret,
) -> Result<EncryptedPayload> {
    if password.is_empty() {
        return Err(Error::Config(
            "an encrypted share needs a nonempty password".to_owned(),
        ));
    }
    let password_text = std::str::from_utf8(password.expose()).map_err(|_| {
        Error::Config("a share password must be valid UTF-8 text for browser decryption".to_owned())
    })?;
    if password_text.chars().any(char::is_control) {
        return Err(Error::Config(
            "a share password must not contain control characters".to_owned(),
        ));
    }
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut salt)
        .map_err(|_| Error::Crypto("secure random salt generation failed".to_owned()))?;
    getrandom::fill(&mut nonce)
        .map_err(|_| Error::Crypto("secure random nonce generation failed".to_owned()))?;
    encrypt_with_material(plaintext, content_type, password, salt, nonce)
}

fn encrypt_with_material(
    plaintext: &[u8],
    content_type: &str,
    password: &Secret,
    salt: [u8; 16],
    nonce: [u8; 12],
) -> Result<EncryptedPayload> {
    let aad = authenticated_aad(content_type)?;
    let mut derived = pbkdf2_hmac_sha256(password.expose(), &salt, PBKDF2_ITERATIONS, 32)
        .ok_or_else(|| Error::Crypto("PBKDF2 parameter overflow".to_owned()))?;
    let mut key = [0u8; 32];
    key.copy_from_slice(&derived);
    derived.fill(0);
    let cipher = Aes256Gcm::new((&key).into());
    let encrypted = cipher.encrypt(
        (&nonce).into(),
        Payload {
            msg: plaintext,
            aad: &aad,
        },
    );
    key.fill(0);
    let ciphertext =
        encrypted.map_err(|_| Error::Crypto("AES-256-GCM encryption failed".to_owned()))?;
    Ok(EncryptedPayload {
        salt,
        nonce,
        ciphertext,
        aad,
        content_type: content_type.to_owned(),
        iterations: PBKDF2_ITERATIONS,
    })
}

/// Renders one HTML object containing the ciphertext, UI and decryption code.
///
/// There is no password endpoint and no server-side password claim. WebCrypto
/// derives and uses the key entirely inside the recipient's browser.
pub fn render_viewer(payload: &EncryptedPayload, branding: &Branding) -> Result<Vec<u8>> {
    branding.validate()?;
    if payload.iterations != PBKDF2_ITERATIONS {
        return Err(Error::Crypto(
            "the encrypted share uses an unsupported PBKDF2 work factor".to_owned(),
        ));
    }
    if payload.ciphertext.len() < 16 {
        return Err(Error::Crypto(
            "the encrypted share is missing its AES-GCM authentication tag".to_owned(),
        ));
    }
    if payload.aad != authenticated_aad(&payload.content_type)? {
        return Err(Error::Crypto(
            "the encrypted share metadata does not match its authenticated data".to_owned(),
        ));
    }
    let title = html_escape(&branding.title);
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src blob: data:; media-src blob:; style-src 'unsafe-inline'; script-src 'unsafe-inline'; form-action 'none'; base-uri 'none'">
<title>{title}</title>
<style>
:root{{--accent:{accent};--on-accent:{accent_ink};color-scheme:light dark}}*{{box-sizing:border-box}}body{{margin:0;min-height:100vh;display:grid;place-items:center;background:#111217;color:#f7f7fa;font:16px system-ui,sans-serif}}main{{width:min(92vw,64rem);text-align:center}}form{{width:min(100%,28rem);margin:auto;padding:2rem;border:1px solid #343641;border-radius:1rem;background:#1b1d24}}input,button{{width:100%;padding:.85rem 1rem;border-radius:.6rem;font:inherit}}input{{border:1px solid #555968;background:#101116;color:inherit}}button{{margin-top:.8rem;border:0;background:var(--accent);color:var(--on-accent);font-weight:700;cursor:pointer}}small{{display:block;margin-top:1rem;color:#b8bac4}}#error{{min-height:1.4em;color:#ff9b9b}}img,video{{display:none;max-width:100%;max-height:92vh;margin:auto;border-radius:.75rem;box-shadow:0 1.5rem 5rem #0008}}
</style>
</head>
<body>
<main>
<form id="unlock">
<h1>{title}</h1>
<p>This capture is encrypted. Enter the password to decrypt it on this device.</p>
<input id="password" type="password" autocomplete="current-password" autofocus required aria-label="Share password">
<button type="submit">Decrypt capture</button>
<p id="error" role="alert"></p>
<small>The password and decrypted capture never leave this browser.</small>
</form>
<img id="capture" alt="Shared capture">
<video id="recording" controls playsinline aria-label="Shared recording"></video>
<noscript>JavaScript is required for local WebCrypto decryption.</noscript>
</main>
<script>
"use strict";
const envelope=Object.freeze({{salt:"{salt}",nonce:"{nonce}",ciphertext:"{ciphertext}",type:"{content_type}",iterations:{iterations}}});
const bytes=value=>Uint8Array.from(atob(value),character=>character.charCodeAt(0));
document.getElementById("unlock").addEventListener("submit",async event=>{{
  event.preventDefault();
  const error=document.getElementById("error");
  error.textContent="";
  try{{
    const mediaType=new TextDecoder().decode(bytes(envelope.type));
    const material=await crypto.subtle.importKey("raw",new TextEncoder().encode(document.getElementById("password").value),"PBKDF2",false,["deriveKey"]);
    const key=await crypto.subtle.deriveKey({{name:"PBKDF2",hash:"SHA-256",salt:bytes(envelope.salt),iterations:envelope.iterations}},material,{{name:"AES-GCM",length:256}},false,["decrypt"]);
    const clear=await crypto.subtle.decrypt({{name:"AES-GCM",iv:bytes(envelope.nonce),additionalData:new TextEncoder().encode("scrozz-share-v1\0"+mediaType)}},key,bytes(envelope.ciphertext));
    const media=mediaType.startsWith("video/")?document.getElementById("recording"):document.getElementById("capture");
    media.src=URL.createObjectURL(new Blob([clear],{{type:mediaType}}));
    media.style.display="block";
    event.currentTarget.remove();
  }}catch(_error){{
    error.textContent="That password could not decrypt this capture.";
  }}
}});
</script>
</body>
</html>
"#,
        accent = branding.accent,
        accent_ink = branding.accent_ink(),
        salt = base64(&payload.salt),
        nonce = base64(&payload.nonce),
        ciphertext = base64(&payload.ciphertext),
        content_type = base64(payload.content_type.as_bytes()),
        iterations = payload.iterations,
    );
    Ok(html.into_bytes())
}

fn authenticated_aad(content_type: &str) -> Result<Vec<u8>> {
    if content_type.trim().is_empty() || content_type.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(Error::Config(
            "encrypted content type must be nonempty and contain no control characters".to_owned(),
        ));
    }
    Ok(format!("scrozz-share-v1\0{content_type}").into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_gcm_round_trip_matches_the_webcrypto_envelope() {
        let password = Secret::from_text("correct horse battery staple");
        let payload =
            encrypt_with_material(b"private pixels", "image/png", &password, [7; 16], [9; 12])
                .unwrap();
        let derived =
            pbkdf2_hmac_sha256(password.expose(), &payload.salt, payload.iterations, 32).unwrap();
        let key: [u8; 32] = derived.try_into().unwrap();
        let cipher = Aes256Gcm::new((&key).into());
        let clear = cipher
            .decrypt(
                (&payload.nonce).into(),
                Payload {
                    msg: &payload.ciphertext,
                    aad: &payload.aad,
                },
            )
            .unwrap();
        assert_eq!(clear, b"private pixels");
    }

    #[test]
    fn viewer_is_self_contained_and_never_claims_server_enforcement() {
        let payload = encrypt_with_material(
            b"pixels",
            "image/png",
            &Secret::from_text("password"),
            [1; 16],
            [2; 12],
        )
        .unwrap();
        // Cross-checked against Node's WebCrypto implementation, not this crate.
        assert_eq!(
            base64(&payload.ciphertext),
            "8Tee+PF7Ni/HNvcoRdLjy3qooEmLsQ=="
        );
        assert_eq!(payload.aad, b"scrozz-share-v1\0image/png");
        let html =
            String::from_utf8(render_viewer(&payload, &Branding::default()).unwrap()).unwrap();
        assert!(html.contains("crypto.subtle.deriveKey"));
        assert!(html.contains("AES-GCM"));
        assert!(html.contains("scrozz-share-v1\\0"));
        assert!(!html.contains(r#"aad:"#));
        assert!(!html.contains("<script src="));
        assert!(!html.contains("fetch("));
        assert!(!html.to_ascii_lowercase().contains("server password"));
    }

    #[test]
    fn encrypted_video_viewer_uses_playable_media_with_a_bounded_csp() {
        let payload = encrypt_with_material(
            b"video",
            "video/webm",
            &Secret::from_text("password"),
            [1; 16],
            [2; 12],
        )
        .unwrap();
        let html =
            String::from_utf8(render_viewer(&payload, &Branding::default()).unwrap()).unwrap();

        assert!(html.contains("<video id=\"recording\" controls"));
        assert!(html.contains("media-src blob:"));
        assert!(html.contains("mediaType.startsWith(\"video/\")"));
    }

    #[test]
    fn branding_is_escaped_not_executed() {
        let payload =
            encrypt_with_material(b"x", "image/png", &Secret::from_text("p"), [1; 16], [2; 12])
                .unwrap();
        let html = String::from_utf8(
            render_viewer(
                &payload,
                &Branding {
                    title: "<script>alert(1)</script>".into(),
                    accent: "#112233".into(),
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert!(!html.contains("<title><script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn viewer_rejects_tampered_envelope_metadata() {
        let mut payload =
            encrypt_with_material(b"x", "image/png", &Secret::from_text("p"), [1; 16], [2; 12])
                .unwrap();
        payload.content_type = "image/jpeg".to_owned();
        assert!(render_viewer(&payload, &Branding::default()).is_err());
    }

    #[test]
    fn browser_passwords_must_have_a_reproducible_text_encoding() {
        assert!(encrypt(b"x", "image/png", &Secret::new(vec![0xff])).is_err());
        assert!(encrypt(b"x", "image/png", &Secret::new(vec![b'p', 0])).is_err());
    }
}
