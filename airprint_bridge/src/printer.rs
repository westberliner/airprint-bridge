//! Talking to the real printer: HTTP client, probing, and what we learn from it.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::UnsyncBoxBody};
use hyper::{Request, header};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;

use crate::config::Options;
use crate::ipp::{self, Message, tag};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Body = UnsyncBoxBody<Bytes, BoxError>;
pub type HttpClient = Client<HttpConnector, Body>;

pub fn client() -> HttpClient {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(Duration::from_secs(10)));
    Client::builder(TokioExecutor::new()).build(connector)
}

pub fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into()).map_err(|never| match never {}).boxed_unsync()
}

/// Where the printer's IPP endpoint is.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Target {
    pub fn authority(&self) -> String {
        match self.host.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V6(ip)) => format!("[{ip}]:{}", self.port),
            _ => format!("{}:{}", self.host, self.port),
        }
    }

    pub fn uri(&self) -> String {
        format!("ipp://{}{}", self.authority(), self.path)
    }
}

/// The current target, updated by discovery when the printer's address changes.
pub type SharedTarget = Arc<RwLock<Target>>;

/// What the bridge advertises, taken from the printer's attributes where it has them.
#[derive(Clone, Debug)]
pub struct PrinterInfo {
    pub make_model: String,
    pub formats: Vec<String>,
    pub urf: Vec<String>,
    pub color: bool,
    pub duplex: bool,
    pub uuid: Option<String>,
    pub max_version: [u8; 2],
    pub admin_url: Option<String>,
    pub location: String,
}

/// URF set for a monochrome 600 dpi laser, used when the printer reports none.
const DEFAULT_URF: &[&str] = &["W8", "RS600", "CP1", "IS1", "MT1", "PQ4", "OB10", "V1.4"];

impl PrinterInfo {
    pub fn fallback(opts: &Options) -> Self {
        PrinterInfo {
            make_model: opts.name.clone(),
            formats: vec!["image/urf".into()],
            urf: DEFAULT_URF.iter().map(|s| s.to_string()).collect(),
            color: false,
            duplex: false,
            uuid: None,
            max_version: [1, 1],
            admin_url: None,
            location: String::new(),
        }
    }

    pub fn from_attributes(msg: &Message, opts: &Options) -> Self {
        let mut info = Self::fallback(opts);
        let get = |name| msg.find(tag::PRINTER, name);
        if let Some(v) = get("printer-make-and-model").and_then(|a| a.first_string()) {
            info.make_model = v;
        }
        if let Some(a) = get("document-format-supported") {
            info.formats = a.strings();
        }
        if let Some(a) = get("urf-supported") {
            info.urf = a.strings();
        }
        if let Some(v) = get("color-supported").and_then(|a| a.boolean()) {
            info.color = v;
        }
        if let Some(a) = get("sides-supported") {
            info.duplex = a.strings().iter().any(|s| s.starts_with("two-sided"));
        }
        info.uuid = get("printer-uuid")
            .and_then(|a| a.first_string())
            .map(|u| u.trim_start_matches("urn:uuid:").to_string());
        if let Some(a) = get("ipp-versions-supported") {
            info.max_version = a
                .strings()
                .iter()
                .filter_map(|v| {
                    let (major, minor) = v.split_once('.')?;
                    Some([major.parse().ok()?, minor.parse().ok()?])
                })
                .max()
                .unwrap_or([1, 1]);
        }
        info.admin_url = get("printer-more-info").and_then(|a| a.first_string());
        info.location = get("printer-location").and_then(|a| a.first_string()).unwrap_or_default();
        info
    }

    pub fn apply_overrides(&mut self, opts: &Options) {
        if let Some(c) = opts.color {
            self.color = c;
        }
        if let Some(d) = opts.duplex {
            self.duplex = d;
        }
        if let Some(urf) = opts.urf_override() {
            self.urf = urf;
        }
    }
}

/// A stable UUID for the bridge's own record, distinct from the printer's,
/// so iOS doesn't merge it with the printer's own (broken) Bonjour record.
pub fn bridge_uuid(seed: &str) -> String {
    let fnv = |offset: u64| {
        seed.bytes().fold(offset, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
    };
    let (a, b) = (fnv(0xcbf29ce484222325), fnv(0x84222325cbf29ce4));
    let hex = format!("{a:016x}{b:016x}");
    // Version 5 / RFC 4122 variant bits, so it looks like any other UUID.
    format!(
        "{}-{}-5{}-a{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[13..16],
        &hex[17..20],
        &hex[20..32]
    )
}

pub async fn post_ipp(client: &HttpClient, target: &Target, body: Vec<u8>) -> Result<Bytes, BoxError> {
    let req = Request::post(format!("http://{}{}", target.authority(), target.path))
        .header(header::CONTENT_TYPE, "application/ipp")
        .body(full(body))?;
    let res = tokio::time::timeout(Duration::from_secs(20), client.request(req)).await??;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()).into());
    }
    Ok(res.into_body().collect().await?.to_bytes())
}

pub async fn get_attributes(client: &HttpClient, target: &Target) -> Result<Message, BoxError> {
    let body = post_ipp(client, target, ipp::get_printer_attributes_request(&target.uri(), 1)).await?;
    let (msg, _) = ipp::parse(&body)?;
    if msg.code > 0x00FF {
        return Err(format!("IPP status 0x{:04x}", msg.code).into());
    }
    Ok(msg)
}

/// Asks the printer for its attributes, retrying for a while in case it is asleep or booting.
pub async fn probe(client: &HttpClient, target: &Target) -> Option<Message> {
    for attempt in 1..=5u64 {
        match get_attributes(client, target).await {
            Ok(msg) => return Some(msg),
            Err(e) => {
                eprintln!("[warn] probing {} failed (attempt {attempt}/5): {e}", target.uri());
                tokio::time::sleep(Duration::from_secs(5 * attempt)).await;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_is_stable_and_well_formed() {
        let a = bridge_uuid("16a65700-007c-1000-bb49-30cda79aaace");
        assert_eq!(a, bridge_uuid("16a65700-007c-1000-bb49-30cda79aaace"));
        assert_ne!(a, bridge_uuid("other"));
        let parts: Vec<_> = a.split('-').map(str::len).collect();
        assert_eq!(parts, [8, 4, 4, 4, 12]);
        assert_eq!(&a[14..15], "5");
    }
}
