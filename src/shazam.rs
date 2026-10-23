//! Minimal client for Shazam's recognition endpoint.

use crate::signature::Signature;
use serde_json::{json, Value};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Track {
    pub key: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub label: Option<String>,
    pub released: Option<String>,
    pub genre: Option<String>,
    pub cover_url: Option<String>,
    pub shazam_url: Option<String>,
    pub apple_music_url: Option<String>,
    /// Position inside the track where the sample matched, in seconds.
    #[serde(default)]
    pub offset_secs: Option<f64>,
    /// Unix seconds when recognised.
    #[serde(default)]
    pub recognised_at: u64,
}

impl Track {
    pub fn query(&self) -> String {
        format!("{} {}", self.artist, self.title)
    }
    fn enc(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                b' ' => "+".into(),
                _ => format!("%{b:02X}"),
            })
            .collect()
    }
    pub fn spotify_url(&self) -> String {
        format!("https://open.spotify.com/search/{}", Self::enc(&self.query()).replace('+', "%20"))
    }
    pub fn youtube_url(&self) -> String {
        format!("https://www.youtube.com/results?search_query={}", Self::enc(&self.query()))
    }
}

const USER_AGENTS: &[&str] = &[
    "Dalvik/2.1.0 (Linux; U; Android 14; Pixel 8 Build/AP2A.240805.005)",
    "Dalvik/2.1.0 (Linux; U; Android 13; SM-G991B Build/TP1A.220624.014)",
    "Dalvik/2.1.0 (Linux; U; Android 12; Pixel 6 Build/SQ3A.220705.004)",
    "Dalvik/2.1.0 (Linux; U; Android 11; SM-A515F Build/RP1A.200720.012)",
];

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let b = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15)));
        #[cfg(any(windows, target_os = "macos"))]
        let b = b.tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        );
        b.build()
            .into()
    })
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// Returns Ok(None) when the service answered but found no match.
pub fn recognise(sig: &Signature) -> Result<Option<Track>, String> {
    let ts = now_ms();
    let url = format!(
        "https://amp.shazam.com/discovery/v5/en/US/android/-/tag/{}/{}",
        uuid::Uuid::new_v4().to_string().to_uppercase(),
        uuid::Uuid::new_v4()
    );
    let body = json!({
        "geolocation": { "altitude": 300, "latitude": 45, "longitude": 2 },
        "signature": {
            "samplems": sig.duration_ms(),
            "timestamp": ts as u32,
            "uri": sig.to_data_uri(),
        },
        "timestamp": ts as u32,
        "timezone": "Europe/London",
    });
    let ua = USER_AGENTS[(ts as usize) % USER_AGENTS.len()];

    let mut resp = agent()
        .post(&url)
        .query("sync", "true")
        .query("webv3", "true")
        .query("sampling", "true")
        .query("connected", "")
        .query("shazamapiversion", "v3")
        .query("sharehub", "true")
        .query("video", "v3")
        .header("User-Agent", ua)
        .header("Content-Language", "en_US")
        .send_json(&body)
        .map_err(|e| format!("network error: {e}"))?;
    let v: Value = resp
        .body_mut()
        .read_json()
        .map_err(|e| format!("bad response: {e}"))?;
    Ok(parse(&v))
}

fn s(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

fn find_apple_music(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if s.starts_with("https://music.apple.com") => Some(s.clone()),
        Value::Array(a) => a.iter().find_map(find_apple_music),
        Value::Object(o) => o.values().find_map(find_apple_music),
        _ => None,
    }
}

fn parse(v: &Value) -> Option<Track> {
    let t = v.get("track")?;
    let title = s(&t["title"])?;
    let meta = |name: &str| -> Option<String> {
        t["sections"].as_array()?.iter().find_map(|sec| {
            sec["metadata"].as_array()?.iter().find_map(|m| {
                (m["title"].as_str()? == name).then(|| s(&m["text"])).flatten()
            })
        })
    };
    let album = meta("Album");
    let label = meta("Label");
    let released = meta("Released");
    Some(Track {
        key: s(&t["key"]).unwrap_or_else(|| title.clone()),
        artist: s(&t["subtitle"]).unwrap_or_default(),
        title,
        album,
        label,
        released,
        genre: s(&t["genres"]["primary"]),
        cover_url: s(&t["images"]["coverarthq"]).or_else(|| s(&t["images"]["coverart"])),
        shazam_url: s(&t["url"]),
        apple_music_url: find_apple_music(&t["hub"]),
        offset_secs: v["matches"][0]["offset"].as_f64(),
        recognised_at: now_ms() / 1000,
    })
}

/// Download raw bytes (used for cover art).
pub fn fetch_bytes(url: &str) -> Result<Vec<u8>, String> {
    agent()
        .get(url)
        .call()
        .map_err(|e| e.to_string())?
        .body_mut()
        .read_to_vec()
        .map_err(|e| e.to_string())
}
