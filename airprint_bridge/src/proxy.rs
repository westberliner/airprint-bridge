//! IPP pass-through: relays requests to the printer, streams job data untouched, and
//! fixes up the attributes the printer reports so current iOS accepts it.

use std::convert::Infallible;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::config::Options;
use crate::ipp::{self, Attribute, Message, OP_GET_PRINTER_ATTRIBUTES, tag};
use crate::printer::{Body, BoxError, HttpClient, SharedTarget, full};

/// Attribute sections are small; anything bigger than this isn't a sane IPP request.
const MAX_HEADER: usize = 256 * 1024;

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "expect",
    "host",
    "content-length",
];

pub struct Proxy {
    pub opts: Options,
    pub target: SharedTarget,
    pub client: HttpClient,
    /// `urn:uuid:…` reported as printer-uuid, matching the Bonjour record.
    pub uuid_urn: String,
    pub urf: Vec<String>,
    /// Highest IPP version the printer speaks; newer requests are sent as this version.
    pub max_version: [u8; 2],
}

impl Proxy {
    pub async fn serve(self: Arc<Self>) -> std::io::Result<()> {
        let listener = match TcpListener::bind(("::", self.opts.port)).await {
            Ok(l) => l,
            Err(_) => TcpListener::bind(("0.0.0.0", self.opts.port)).await?,
        };
        eprintln!("[info] IPP proxy listening on port {}", self.opts.port);
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("[warn] accept failed: {e}");
                    continue;
                }
            };
            let proxy = self.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let proxy = proxy.clone();
                    async move { Ok::<_, Infallible>(proxy.handle(req).await) }
                });
                if let Err(e) = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await {
                    eprintln!("[debug] connection closed: {e}");
                }
            });
        }
    }

    async fn handle(&self, req: Request<Incoming>) -> Response<Body> {
        self.forward(req).await.unwrap_or_else(|e| {
            eprintln!("[warn] relaying to printer failed: {e}");
            let mut res = Response::new(full("printer unreachable"));
            *res.status_mut() = StatusCode::BAD_GATEWAY;
            res
        })
    }

    async fn forward(&self, req: Request<Incoming>) -> Result<Response<Body>, BoxError> {
        let client_authority = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .map(str::to_string)
            .unwrap_or_else(|| format!("airprint-bridge.local:{}", self.opts.port));
        let is_ipp = req.method() == Method::POST
            && req
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok())
                .is_some_and(|ct| ct.starts_with("application/ipp"));

        let (parts, mut body) = req.into_parts();
        let printer_authority = self.printer_authority();
        let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
        let mut out = Request::builder().method(parts.method.clone()).uri(format!("http://{printer_authority}{path}"));
        let headers = out.headers_mut().expect("valid request builder");
        copy_headers(&parts.headers, headers);
        headers.insert(header::HOST, HeaderValue::from_str(&printer_authority)?);
        let content_length = parts
            .headers
            .get(header::CONTENT_LENGTH)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.parse::<usize>().ok());

        if !is_ipp {
            if let Some(len) = content_length {
                headers.insert(header::CONTENT_LENGTH, len.into());
            }
            let res = self.client.request(out.body(body.map_err(BoxError::from).boxed_unsync())?).await?;
            let (mut res_parts, res_body) = res.into_parts();
            strip_hop_by_hop(&mut res_parts.headers);
            return Ok(Response::from_parts(res_parts, res_body.map_err(BoxError::from).boxed_unsync()));
        }

        // IPP: rewrite the attribute section, then stream the document data behind it as is.
        let (mut msg, offset, buffered) = read_header(&mut body).await?;
        let (operation, client_version) = (msg.code, msg.version);
        self.rewrite_request(&mut msg);
        let mut head = msg.encode();
        if let Some(len) = content_length.and_then(|len| len.checked_sub(offset)) {
            headers.insert(header::CONTENT_LENGTH, (len + head.len()).into());
        }
        head.extend_from_slice(&buffered[offset..]);
        let first = futures_util::stream::iter([Ok::<_, BoxError>(Frame::data(Bytes::from(head)))]);
        let rest = body.into_data_stream().map(|r| r.map(Frame::data).map_err(BoxError::from));
        let res = self.client.request(out.body(StreamBody::new(first.chain(rest)).boxed_unsync())?).await?;

        // IPP responses are small; buffer, rewrite, re-encode.
        let (mut res_parts, res_body) = res.into_parts();
        let bytes = res_body.collect().await?.to_bytes();
        let bytes = match ipp::parse(&bytes) {
            Ok((mut msg, data)) => {
                msg.version = client_version;
                self.rewrite_response(&mut msg, operation, &client_authority);
                let mut out = msg.encode();
                out.extend_from_slice(data);
                Bytes::from(out)
            }
            Err(e) => {
                eprintln!("[warn] passing through unparseable printer response: {e}");
                bytes
            }
        };
        strip_hop_by_hop(&mut res_parts.headers);
        res_parts.headers.insert(header::CONTENT_LENGTH, bytes.len().into());
        Ok(Response::from_parts(res_parts, full(bytes)))
    }

    fn printer_authority(&self) -> String {
        self.target.read().expect("target lock").authority()
    }

    /// Points the request at the printer and speaks an IPP version it understands.
    fn rewrite_request(&self, msg: &mut Message) {
        msg.version = msg.version.min(self.max_version);
        let authority = self.printer_authority();
        for attr in msg.attrs_mut().filter(|a| a.name == "printer-uri" || a.name == "job-uri") {
            rewrite_uris(attr, &authority);
        }
    }

    /// Points every IPP URI in the response back at the bridge, and patches printer attributes.
    fn rewrite_response(&self, msg: &mut Message, operation: u16, client_authority: &str) {
        for attr in msg.attrs_mut() {
            rewrite_uris(attr, client_authority);
        }
        if operation == OP_GET_PRINTER_ATTRIBUTES && msg.code <= 0x00FF {
            self.patch_printer_attributes(msg);
        }
    }

    fn patch_printer_attributes(&self, msg: &mut Message) {
        let p = tag::PRINTER;
        msg.set(p, Attribute::new("printer-uuid", tag::URI, [self.uuid_urn.as_str()]));
        msg.set(p, Attribute::new("printer-dns-sd-name", tag::NAME, [self.opts.name.as_str()]));

        let mut formats = msg.find(p, "document-format-supported").map(Attribute::strings).unwrap_or_default();
        if !formats.iter().any(|f| f == "image/urf") {
            formats.push("image/urf".into());
            msg.set(p, Attribute::new("document-format-supported", tag::MIME, formats));
        }
        if msg.find(p, "urf-supported").is_none() {
            msg.set(p, Attribute::new("urf-supported", tag::KEYWORD, self.urf.clone()));
        }

        let mut versions = msg.find(p, "ipp-versions-supported").map(Attribute::strings).unwrap_or_default();
        for v in ["1.1", "2.0"] {
            if !versions.iter().any(|x| x == v) {
                versions.push(v.into());
            }
        }
        msg.set(p, Attribute::new("ipp-versions-supported", tag::KEYWORD, versions));

        // Every URI now points at the bridge, so the printer's IPv4/IPv6/hostname variants collapse
        // into duplicates. Keep one of each, with the per-URI lists in step.
        if let Some(uris) = msg.find(p, "printer-uri-supported").cloned() {
            let mut keep: Vec<usize> = Vec::new();
            for (i, v) in uris.values.iter().enumerate() {
                if keep.iter().all(|&k| uris.values[k] != *v) {
                    keep.push(i);
                }
            }
            let pick = |a: &Attribute| Attribute {
                name: a.name.clone(),
                values: keep.iter().filter_map(|&i| a.values.get(i).cloned()).collect(),
            };
            msg.set(p, pick(&uris));
            if let Some(auth) = msg.find(p, "uri-authentication-supported").cloned() {
                msg.set(p, pick(&auth));
            }
            msg.set(p, Attribute::new("uri-security-supported", tag::KEYWORD, vec!["none"; keep.len()]));
        }
    }
}

async fn read_header(body: &mut Incoming) -> Result<(Message, usize, BytesMut), BoxError> {
    let mut buf = BytesMut::new();
    loop {
        if let Some((msg, offset)) = ipp::parse_header(&buf)? {
            return Ok((msg, offset, buf));
        }
        if buf.len() > MAX_HEADER {
            return Err("IPP attribute section too large".into());
        }
        match body.frame().await {
            Some(frame) => {
                if let Ok(data) = frame?.into_data() {
                    buf.extend_from_slice(&data);
                }
            }
            None => return Err("request ended inside the IPP attribute section".into()),
        }
    }
}

fn rewrite_uris(attr: &mut Attribute, authority: &str) {
    for (t, v) in &mut attr.values {
        if *t == tag::URI && (v.starts_with(b"ipp://") || v.starts_with(b"ipps://")) {
            *v = ipp::replace_authority(&String::from_utf8_lossy(v), "ipp", authority).into_bytes();
        }
    }
}

fn copy_headers(from: &HeaderMap, to: &mut HeaderMap) {
    for (name, value) in from {
        if !HOP_BY_HOP.contains(&name.as_str()) {
            to.append(name, value.clone());
        }
    }
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipp::Group;

    fn proxy() -> Proxy {
        let opts: Options = serde_json::from_str(r#"{"printer_host":"192.168.178.200","name":"Samsung (AirPrint)"}"#).unwrap();
        let target = opts.manual_target().unwrap();
        Proxy {
            opts,
            target: std::sync::Arc::new(std::sync::RwLock::new(target)),
            client: crate::printer::client(),
            uuid_urn: "urn:uuid:11111111-2222-5333-a444-555555555555".into(),
            urf: vec!["W8".into(), "RS600".into()],
            max_version: [1, 1],
        }
    }

    fn gpa_response() -> Message {
        let mut msg = Message::new([1, 1], 0x0000, 1);
        msg.groups.push(Group {
            tag: tag::OPERATION,
            attrs: vec![Attribute::new("attributes-charset", tag::CHARSET, ["utf-8"])],
        });
        msg.groups.push(Group {
            tag: tag::PRINTER,
            attrs: vec![
                Attribute::new(
                    "printer-uri-supported",
                    tag::URI,
                    ["ipp://192.168.178.200/ipp/print", "ipp://SEC30CDA79AAACE.local./ipp/print"],
                ),
                Attribute::new("uri-security-supported", tag::KEYWORD, ["none", "none"]),
                Attribute::new("uri-authentication-supported", tag::KEYWORD, ["none", "none"]),
                Attribute::new("document-format-supported", tag::MIME, ["application/PCL"]),
                Attribute::new("ipp-versions-supported", tag::KEYWORD, ["1.0", "1.1"]),
                Attribute::new("printer-uuid", tag::URI, ["urn:uuid:16a65700-007c-1000-bb49-30cda79aaace"]),
            ],
        });
        msg
    }

    #[test]
    fn request_is_pointed_at_printer_and_downgraded() {
        let p = proxy();
        let mut msg = Message::new([2, 0], 0x0002, 3);
        msg.groups.push(Group {
            tag: tag::OPERATION,
            attrs: vec![Attribute::new("printer-uri", tag::URI, ["ipp://airprint-bridge.local:631/ipp/print"])],
        });
        p.rewrite_request(&mut msg);
        assert_eq!(msg.version, [1, 1]);
        assert_eq!(
            msg.find(tag::OPERATION, "printer-uri").unwrap().strings(),
            ["ipp://192.168.178.200:631/ipp/print"]
        );
    }

    #[test]
    fn printer_attributes_are_patched() {
        let p = proxy();
        let mut msg = gpa_response();
        p.rewrite_response(&mut msg, OP_GET_PRINTER_ATTRIBUTES, "homeassistant.local:631");
        let get = |n| msg.find(tag::PRINTER, n).unwrap().strings();
        assert_eq!(get("printer-uri-supported"), ["ipp://homeassistant.local:631/ipp/print"]);
        assert_eq!(get("uri-security-supported"), ["none"]);
        assert_eq!(get("uri-authentication-supported"), ["none"]);
        assert_eq!(get("document-format-supported"), ["application/PCL", "image/urf"]);
        assert_eq!(get("urf-supported"), ["W8", "RS600"]);
        assert_eq!(get("ipp-versions-supported"), ["1.0", "1.1", "2.0"]);
        assert_eq!(get("printer-uuid"), ["urn:uuid:11111111-2222-5333-a444-555555555555"]);
        assert_eq!(get("printer-dns-sd-name"), ["Samsung (AirPrint)"]);
    }

    #[test]
    fn other_responses_only_get_uris_rewritten() {
        let p = proxy();
        let mut msg = gpa_response();
        p.rewrite_response(&mut msg, 0x0002, "ha:631");
        assert!(msg.find(tag::PRINTER, "urf-supported").is_none());
        assert_eq!(
            msg.find(tag::PRINTER, "printer-uri-supported").unwrap().strings(),
            ["ipp://ha:631/ipp/print", "ipp://ha:631/ipp/print"]
        );
    }
}
