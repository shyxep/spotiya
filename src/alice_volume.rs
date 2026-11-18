//! Keeps Alice's own volume level in step with the Soloist volume.
//!
//! Alice keeps her level in `/data/quasar/last_vendor_volume` (hundredths of a
//! percent) and answers "what is the volume" from it. Writing Master does not
//! tell her, so the level is set through the local Glagol WebSocket
//! (`wss://127.0.0.1:1961`, the Yandex app's local control): its `setVolume`
//! goes through Alice's VolumeCapability, which also drives Master by her curve.
//! Glagol accepts a conversation token that quasar.yandex.net issues for the
//! account OAuth token authd hands to local clients. Tokens are never logged
//! or stored on disk.

use futures_util::{SinkExt, StreamExt};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::CryptoProvider,
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs, io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU16, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncReadExt, net::TcpStream, sync::watch, time::Instant};
use tokio_tungstenite::{Connector, connect_async_tls_with_config, tungstenite::Message};

const LEVEL: &str = "/data/quasar/last_vendor_volume";
const AUTHD: &str = "127.0.0.1:9878";
const ALICED: &str = "127.0.0.1:9875";
const GLAGOL: &str = "wss://127.0.0.1:1961";
const TOKEN_URL: &str = "https://quasar.yandex.net/glagol/token";
// Conversation tokens live a day; renew well before, or at once after a refusal.
const TOKEN_LIFETIME: Duration = Duration::from_secs(12 * 3600);
// A slider drag sends a burst of values; only the last one is sent to Alice.
const DEBOUNCE: Duration = Duration::from_millis(250);
// Level changes this soon after our own setVolume are its echo, not the user's.
const ECHO: Duration = Duration::from_millis(1500);
const UNKNOWN: u16 = u16::MAX;
const OPT_OUT: &str = "SPOTIYA_ALICE_VOLUME";

/// Alice's level in percent, as she last stored it.
pub fn level() -> Option<u16> {
    let value: u32 = fs::read_to_string(LEVEL).ok()?.trim().parse().ok()?;
    Some((value.saturating_add(50) / 100).min(100) as u16)
}

pub struct AliceVolume {
    enabled: bool,
    target: watch::Sender<Option<u16>>,
    seen: AtomicU16,
    quiet_until: Mutex<Option<Instant>>,
}
impl AliceVolume {
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            enabled: false,
            target: watch::channel(None).0,
            seen: AtomicU16::new(UNKNOWN),
            quiet_until: Mutex::new(None),
        })
    }
    /// Starts the Glagol worker when Soloist attenuates in software (Mini 1) and so leaves
    /// Master to Alice, and her level is readable. `SPOTIYA_ALICE_VOLUME=0` opts out.
    pub fn start(software_volume: bool) -> Arc<Self> {
        let opted_out = std::env::var_os(OPT_OUT).is_some_and(|v| v == "0");
        if !software_volume || opted_out {
            return Self::disabled();
        }
        let Some(current) = level() else {
            return Self::disabled();
        };
        let (target, receiver) = watch::channel(None);
        let this = Arc::new(Self {
            enabled: true,
            target,
            seen: AtomicU16::new(current),
            quiet_until: Mutex::new(None),
        });
        tokio::spawn(worker(this.clone(), receiver));
        this
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    /// Asks Alice to take this level; the latest request wins.
    pub fn request(&self, percent: u16) {
        if self.enabled {
            self.quiet();
            self.target.send_replace(Some(percent.min(100)));
        }
    }
    /// Alice's level when she changed it herself (voice, buttons, the Yandex app).
    pub fn changed_by_alice(&self) -> Option<u16> {
        let current = level()?;
        if self.seen.swap(current, Ordering::AcqRel) == current {
            return None;
        }
        let quiet = self
            .quiet_until
            .lock()
            .ok()
            .and_then(|until| *until)
            .is_some_and(|until| Instant::now() < until);
        (!quiet).then_some(current)
    }
    fn quiet(&self) {
        if let Ok(mut until) = self.quiet_until.lock() {
            *until = Some(Instant::now() + ECHO);
        }
    }
}

async fn worker(volume: Arc<AliceVolume>, mut target: watch::Receiver<Option<u16>>) {
    let mut glagol = Glagol::default();
    let mut failing = false;
    while target.changed().await.is_ok() {
        tokio::time::sleep(DEBOUNCE).await;
        let Some(percent) = *target.borrow_and_update() else {
            continue;
        };
        if level() == Some(percent) {
            continue;
        }
        volume.quiet();
        let mut result = glagol.set_volume(percent).await;
        if result.is_err() {
            // An expired or revoked conversation token is refused; renew it once.
            glagol.token = None;
            result = glagol.set_volume(percent).await;
        }
        volume.quiet();
        match result {
            Ok(()) if failing => {
                failing = false;
                eprintln!("alice volume: Glagol works again");
            }
            Ok(()) => {}
            Err(e) if !failing => {
                failing = true;
                eprintln!("alice volume: {e}");
            }
            Err(_) => {}
        }
    }
}

#[derive(Deserialize)]
struct TokenReply {
    status: String,
    #[serde(default)]
    token: String,
}

#[derive(Default)]
struct Glagol {
    token: Option<(String, Instant)>,
    device: Option<(String, String)>,
}
impl Glagol {
    async fn token(&mut self) -> io::Result<String> {
        if let Some((token, at)) = &self.token
            && at.elapsed() < TOKEN_LIFETIME
        {
            return Ok(token.clone());
        }
        if self.device.is_none() {
            self.device = Some(device().await?);
        }
        let (id, platform) = self.device.clone().unwrap_or_default();
        let oauth = oauth().await?;
        let body = client()?
            .get(format!("{TOKEN_URL}?device_id={id}&platform={platform}"))
            .header(reqwest::header::AUTHORIZATION, format!("OAuth {oauth}"))
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|_| io::Error::other("Glagol token request failed"))?
            .bytes()
            .await
            .map_err(|_| io::Error::other("Glagol token request failed"))?;
        let reply = serde_json::from_slice::<TokenReply>(&body)
            .ok()
            .filter(|reply| reply.status == "ok" && !reply.token.is_empty())
            .ok_or_else(|| io::Error::other("Glagol token request was refused"))?;
        self.token = Some((reply.token.clone(), Instant::now()));
        Ok(reply.token)
    }
    async fn set_volume(&mut self, percent: u16) -> io::Result<()> {
        let token = self.token().await?;
        let (mut socket, _) = tokio::time::timeout(
            Duration::from_secs(5),
            connect_async_tls_with_config(GLAGOL, None, false, Some(connector()?)),
        )
        .await
        .map_err(|_| io::Error::other("Glagol connection timed out"))?
        .map_err(|e| io::Error::other(format!("Glagol connection failed: {e}")))?;
        let id = uuid::Uuid::new_v4().to_string();
        let sent = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let message = json!({
            "conversationToken": token,
            "id": id,
            "sentTime": sent,
            "payload": {"command": "setVolume", "volume": f64::from(percent) / 100.0},
        });
        let reply = tokio::time::timeout(Duration::from_secs(3), async {
            socket
                .send(Message::Text(message.to_string().into()))
                .await
                .map_err(io::Error::other)?;
            while let Some(message) = socket.next().await {
                if let Message::Text(text) = message.map_err(io::Error::other)?
                    && let Ok(reply) = serde_json::from_str::<Value>(&text)
                    && reply["requestId"] == id.as_str()
                {
                    return Ok(reply);
                }
            }
            Err::<Value, _>(io::Error::other("Glagol closed the connection"))
        })
        .await
        .map_err(|_| io::Error::other("Glagol did not answer"));
        let _ = socket.close(None).await;
        let reply = reply??;
        match reply["status"].as_str() {
            Some("SUCCESS") => Ok(()),
            status => Err(io::Error::other(format!(
                "Glagol refused setVolume: {} {}",
                status.unwrap_or("no status"),
                reply["errorCode"].as_str().unwrap_or("")
            ))),
        }
    }
}

fn client() -> io::Result<reqwest::Client> {
    reqwest::Client::builder()
        .https_only(true)
        .user_agent(concat!("spotiya/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| io::Error::other("Cannot initialize HTTPS client"))
}

/// Reads the greeting a local Quasar service sends to every client until `pick` finds a value.
async fn greeting<T>(address: &str, mut pick: impl FnMut(&[u8]) -> Option<T>) -> io::Result<T> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut socket = TcpStream::connect(address).await?;
        let mut data = Vec::new();
        loop {
            let size = socket.read_u32_le().await? as usize;
            if size == 0 || size > 256 * 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "native frame length",
                ));
            }
            data.resize(size, 0);
            socket.read_exact(&mut data).await?;
            if let Some(value) = pick(&data) {
                return Ok(value);
            }
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "native greeting timeout"))?
}

async fn oauth() -> io::Result<String> {
    greeting(AUTHD, oauth_token)
        .await
        .map_err(|_| io::Error::other("Alice account token is unavailable"))
}

async fn device() -> io::Result<(String, String)> {
    greeting(ALICED, device_info)
        .await
        .map_err(|_| io::Error::other("Alice device id is unavailable"))
}

// authd greets with the account (type 221); field 2 is the OAuth token Glagol accepts.
fn oauth_token(data: &[u8]) -> Option<String> {
    use crate::voice::{bytes, number};
    if number(data, 253) != Some(221) {
        return None;
    }
    let token = std::str::from_utf8(bytes(bytes(data, 252)?, 2)?).ok()?;
    (!token.is_empty()).then(|| token.to_owned())
}

// aliced greets with device info (type 207); 252.1.1 holds the id (6) and platform (11).
fn device_info(data: &[u8]) -> Option<(String, String)> {
    use crate::voice::{bytes, number};
    if number(data, 253) != Some(207) {
        return None;
    }
    let info = bytes(bytes(bytes(data, 252)?, 1)?, 1)?;
    let text = |field| {
        let value = std::str::from_utf8(bytes(info, field)?).ok()?;
        plain(value).then(|| value.to_owned())
    };
    Some((text(6)?, text(11)?))
}

// The id and platform go into a URL query unescaped.
fn plain(value: &str) -> bool {
    let allowed = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
    !value.is_empty() && value.bytes().all(allowed)
}

fn connector() -> io::Result<Connector> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(LocalSpeaker(provider)))
        .with_no_client_auth();
    Ok(Connector::Rustls(Arc::new(config)))
}

/// Glagol presents a per-device self-signed certificate and is reached only on loopback,
/// so the chain is not checked; handshake signatures still are.
#[derive(Debug)]
struct LocalSpeaker(Arc<CryptoProvider>);
impl ServerCertVerifier for LocalSpeaker {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(out: &mut Vec<u8>, field: u64, value: &[u8]) {
        out.push(((field << 3) | 2) as u8);
        out.push(value.len() as u8);
        out.extend_from_slice(value);
    }
    fn var(out: &mut Vec<u8>, mut v: u64) {
        while v >= 128 {
            out.push((v as u8 & 127) | 128);
            v >>= 7;
        }
        out.push(v as u8);
    }
    // QuasarMessage {252: payload, 253: kind}
    fn message(kind: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        var(&mut out, (252 << 3) | 2);
        var(&mut out, payload.len() as u64);
        out.extend_from_slice(payload);
        var(&mut out, 253 << 3);
        var(&mut out, kind);
        out
    }

    #[test]
    fn authd_greeting_yields_the_account_token() {
        let mut account = Vec::new();
        put(&mut account, 2, b"y0_token");
        put(&mut account, 4, b"y0_other");
        assert_eq!(
            oauth_token(&message(221, &account)).as_deref(),
            Some("y0_token")
        );
        assert_eq!(oauth_token(&message(222, &account)), None);
    }

    #[test]
    fn aliced_greeting_yields_device_id_and_platform() {
        let mut info = Vec::new();
        put(&mut info, 1, b"aliced");
        put(&mut info, 6, b"FF98F029A24EDE0547F6B566");
        put(&mut info, 11, b"yandexmini");
        let mut outer = Vec::new();
        put(&mut outer, 1, &info);
        let mut state = Vec::new();
        put(&mut state, 1, &outer);
        assert_eq!(
            device_info(&message(207, &state)),
            Some(("FF98F029A24EDE0547F6B566".into(), "yandexmini".into()))
        );
        let mut bad = Vec::new();
        put(&mut bad, 6, b"id&x=1");
        put(&mut bad, 11, b"yandexmini");
        let mut outer = Vec::new();
        put(&mut outer, 1, &bad);
        let mut state = Vec::new();
        put(&mut state, 1, &outer);
        assert_eq!(device_info(&message(207, &state)), None);
    }

    #[test]
    fn disabled_sync_ignores_requests() {
        let volume = AliceVolume::disabled();
        assert!(!volume.enabled());
        volume.request(30);
        assert_eq!(*volume.target.borrow(), None);
    }
}
