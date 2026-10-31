//! AirScan (eSCL) server in front of the Samsung scanner, for macOS and eSCL-capable apps.
//!
//! Endpoints: `GET /eSCL/ScannerCapabilities`, `GET /eSCL/ScannerStatus`, `POST /eSCL/ScanJobs`,
//! `GET /eSCL/ScanJobs/<id>/NextDocument`, `DELETE /eSCL/ScanJobs/<id>`.
//! Sizes in eSCL are in 1/300 inch; the scanner works in 1/1200 inch.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::{Notify, OwnedMutexGuard, mpsc};

use crate::image::Document;
use crate::printer::{Body, SharedTarget, bridge_uuid, full};
use crate::scanner::{self, Capabilities, ScanError, ScanParams};

const NS: &str = r#"xmlns:scan="http://schemas.hp.com/imaging/escl/2011/05/03" xmlns:pwg="http://www.pwg.org/schemas/2010/12/sm""#;
const VERSION: &str = "2.63";
/// Resolutions offered to clients, if the scanner has them.
const RESOLUTIONS: [u32; 6] = [75, 100, 150, 200, 300, 600];
/// Finished jobs stay listed in ScannerStatus this long.
const JOB_TTL: Duration = Duration::from_secs(600);
/// How long NextDocument waits for the next page (slow 600 dpi scans included).
const PAGE_WAIT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq)]
enum JobState {
    Processing,
    Completed,
    Canceled,
    Aborted,
}

struct Job {
    state: JobState,
    reason: &'static str,
    pages: VecDeque<Document>,
    last: Option<(u32, u32, u8)>,
    images_completed: u32,
    created: Instant,
}

pub struct Escl {
    pub uuid: String,
    pub port: u16,
    pub admin_url: Option<String>,
    target: SharedTarget,
    caps: Mutex<Capabilities>,
    jobs: Mutex<HashMap<String, Job>>,
    changed: Notify,
    /// One scan at a time; held by the running job.
    scanner_lock: Arc<tokio::sync::Mutex<()>>,
    counter: AtomicU32,
}

impl Escl {
    pub fn new(uuid: String, port: u16, admin_url: Option<String>, target: SharedTarget, caps: Capabilities) -> Self {
        Escl {
            uuid,
            port,
            admin_url,
            target,
            caps: Mutex::new(caps),
            jobs: Mutex::new(HashMap::new()),
            changed: Notify::new(),
            scanner_lock: Arc::new(tokio::sync::Mutex::new(())),
            counter: AtomicU32::new(0),
        }
    }

    pub fn caps(&self) -> Capabilities {
        self.caps.lock().expect("caps lock").clone()
    }

    pub async fn serve(self: Arc<Self>) -> std::io::Result<()> {
        let listener = match TcpListener::bind(("::", self.port)).await {
            Ok(l) => l,
            Err(_) => TcpListener::bind(("0.0.0.0", self.port)).await?,
        };
        eprintln!("[info] AirScan (eSCL) listening on port {}", self.port);
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("[warn] accept failed: {e}");
                    continue;
                }
            };
            let escl = self.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let escl = escl.clone();
                    async move { Ok::<_, Infallible>(escl.handle(req).await) }
                });
                let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
            });
        }
    }

    async fn handle(self: Arc<Self>, req: Request<Incoming>) -> Response<Body> {
        let path = req.uri().path().trim_end_matches('/').to_string();
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match (req.method().clone(), parts.as_slice()) {
            (Method::GET, ["eSCL", "ScannerCapabilities"]) => xml(StatusCode::OK, self.capabilities_xml()),
            (Method::GET, ["eSCL", "ScannerStatus"]) => {
                self.refresh_adf().await;
                xml(StatusCode::OK, self.status_xml())
            }
            (Method::POST, ["eSCL", "ScanJobs"]) => {
                let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).map(str::to_string);
                let body = match req.into_body().collect().await {
                    Ok(b) => b.to_bytes(),
                    Err(_) => return status(StatusCode::BAD_REQUEST),
                };
                self.create_job(&String::from_utf8_lossy(&body), host).await
            }
            (Method::GET, ["eSCL", "ScanJobs", id, "NextDocument"]) => self.next_document(id).await,
            (Method::GET, ["eSCL", "ScanJobs", id, "ScanImageInfo"]) => self.image_info(id),
            (Method::DELETE, ["eSCL", "ScanJobs", id]) => self.cancel(id),
            _ => status(StatusCode::NOT_FOUND),
        }
    }

    /// Updates the "paper in feeder" flag, unless a scan is using the scanner.
    async fn refresh_adf(&self) {
        let Ok(_guard) = self.scanner_lock.try_lock() else { return };
        let host = self.target.read().expect("target lock").host.clone();
        if let Ok(Ok(caps)) = tokio::time::timeout(Duration::from_secs(3), scanner::capabilities(&host, scanner::PORT)).await {
            *self.caps.lock().expect("caps lock") = caps;
        }
    }

    async fn create_job(self: &Arc<Self>, settings: &str, host: Option<String>) -> Response<Body> {
        let Ok(guard) = self.scanner_lock.clone().try_lock_owned() else {
            return status(StatusCode::SERVICE_UNAVAILABLE);
        };
        let caps = self.caps();
        let (params, pdf) = parse_settings(settings, &caps);
        eprintln!(
            "[info] scan job: {} dpi, {}, {}, {}",
            params.dpi,
            if params.color { "color" } else { "gray" },
            if params.adf { "feeder" } else { "glass" },
            if pdf { "PDF" } else { "JPEG" }
        );

        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let id = bridge_uuid(&format!("{}-{n}-{:?}", self.uuid, Instant::now()));
        {
            let mut jobs = self.jobs.lock().expect("jobs lock");
            jobs.retain(|_, j| j.state == JobState::Processing || j.created.elapsed() < JOB_TTL);
            jobs.insert(
                id.clone(),
                Job {
                    state: JobState::Processing,
                    reason: "JobScanning",
                    pages: VecDeque::new(),
                    last: None,
                    images_completed: 0,
                    created: Instant::now(),
                },
            );
        }
        tokio::spawn(self.clone().run_job(id.clone(), params, pdf, guard));

        let host = host.unwrap_or_else(|| format!("airprint-bridge.local:{}", self.port));
        let mut res = status(StatusCode::CREATED);
        if let Ok(location) = HeaderValue::from_str(&format!("http://{host}/eSCL/ScanJobs/{id}")) {
            res.headers_mut().insert(header::LOCATION, location);
        }
        res
    }

    async fn run_job(self: Arc<Self>, id: String, params: ScanParams, pdf: bool, _guard: OwnedMutexGuard<()>) {
        let host = self.target.read().expect("target lock").host.clone();
        let (tx, mut rx) = mpsc::channel(1);
        let scan = tokio::spawn(async move { scanner::scan(&host, scanner::PORT, params, tx).await });

        let mut error = None;
        while let Some(page) = rx.recv().await {
            if self.job_state(&id) != Some(JobState::Processing) {
                break; // cancelled: dropping the receiver aborts the scan after this page
            }
            match tokio::task::spawn_blocking(move || Document::from_page(page, &params, pdf)).await {
                Ok(Ok(doc)) => {
                    eprintln!("[info] scanned page: {} ({} bytes)", doc.describe(), doc.bytes.len());
                    let mut jobs = self.jobs.lock().expect("jobs lock");
                    if let Some(job) = jobs.get_mut(&id) {
                        job.images_completed += 1;
                        job.pages.push_back(doc);
                    }
                    drop(jobs);
                    self.changed.notify_waiters();
                }
                Ok(Err(e)) => {
                    error = Some(e);
                    break;
                }
                Err(e) => {
                    error = Some(e.to_string());
                    break;
                }
            }
        }
        drop(rx);
        let result = scan.await;

        let mut jobs = self.jobs.lock().expect("jobs lock");
        if let Some(job) = jobs.get_mut(&id) {
            if job.state == JobState::Processing {
                (job.state, job.reason) = match (&error, result) {
                    (Some(e), _) => {
                        eprintln!("[warn] scan failed: {e}");
                        (JobState::Aborted, "ErrorsDetected")
                    }
                    (None, Ok(Ok(()))) => (JobState::Completed, "JobCompletedSuccessfully"),
                    (None, Ok(Err(e))) => {
                        eprintln!("[warn] scan failed: {e}");
                        let reason = match e {
                            ScanError::NoDocument => "ResourcesAreNotReady",
                            ScanError::Cancelled => "JobCanceledByUser",
                            _ => "ErrorsDetected",
                        };
                        (JobState::Aborted, reason)
                    }
                    (None, Err(e)) => {
                        eprintln!("[warn] scan task failed: {e}");
                        (JobState::Aborted, "ErrorsDetected")
                    }
                };
            }
        }
        drop(jobs);
        self.changed.notify_waiters();
    }

    fn job_state(&self, id: &str) -> Option<JobState> {
        self.jobs.lock().expect("jobs lock").get(id).map(|j| j.state)
    }

    async fn next_document(&self, id: &str) -> Response<Body> {
        let deadline = tokio::time::Instant::now() + PAGE_WAIT;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut jobs = self.jobs.lock().expect("jobs lock");
                let Some(job) = jobs.get_mut(id) else { return status(StatusCode::NOT_FOUND) };
                if let Some(doc) = job.pages.pop_front() {
                    job.last = Some((doc.width, doc.height, doc.channels));
                    let mut res = Response::new(full(doc.bytes));
                    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(doc.content_type));
                    return res;
                }
                match job.state {
                    JobState::Processing => {}
                    JobState::Aborted if job.images_completed == 0 => return status(StatusCode::CONFLICT),
                    _ => return status(StatusCode::NOT_FOUND),
                }
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return status(StatusCode::SERVICE_UNAVAILABLE);
            }
        }
    }

    fn image_info(&self, id: &str) -> Response<Body> {
        let jobs = self.jobs.lock().expect("jobs lock");
        let Some((w, h, c)) = jobs.get(id).and_then(|j| j.last) else { return status(StatusCode::NOT_FOUND) };
        xml(
            StatusCode::OK,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<scan:ImageInfo {NS}><pwg:JobUri>/eSCL/ScanJobs/{id}</pwg:JobUri><pwg:JobUuid>{id}</pwg:JobUuid><scan:ActualWidth>{w}</scan:ActualWidth><scan:ActualHeight>{h}</scan:ActualHeight><scan:ActualBytesPerLine>{}</scan:ActualBytesPerLine></scan:ImageInfo>"#,
                w * c as u32
            ),
        )
    }

    fn cancel(&self, id: &str) -> Response<Body> {
        let mut jobs = self.jobs.lock().expect("jobs lock");
        let Some(job) = jobs.get_mut(id) else { return status(StatusCode::NOT_FOUND) };
        if job.state == JobState::Processing {
            job.state = JobState::Canceled;
            job.reason = "JobCanceledByUser";
        }
        job.pages.clear();
        drop(jobs);
        self.changed.notify_waiters();
        status(StatusCode::OK)
    }

    fn capabilities_xml(&self) -> String {
        let caps = self.caps();
        let resolutions: Vec<u32> = RESOLUTIONS.iter().copied().filter(|r| caps.resolutions.contains(r)).collect();
        let max_dpi = resolutions.iter().max().copied().unwrap_or(300);
        let input = |max_len: u32| {
            let res: String = resolutions
                .iter()
                .map(|r| format!("<scan:DiscreteResolution><scan:XResolution>{r}</scan:XResolution><scan:YResolution>{r}</scan:YResolution></scan:DiscreteResolution>"))
                .collect();
            format!(
                "<scan:MinWidth>16</scan:MinWidth><scan:MaxWidth>{}</scan:MaxWidth>\
                 <scan:MinHeight>16</scan:MinHeight><scan:MaxHeight>{}</scan:MaxHeight>\
                 <scan:MaxScanRegions>1</scan:MaxScanRegions>\
                 <scan:SettingProfiles><scan:SettingProfile>\
                 <scan:ColorModes><scan:ColorMode>RGB24</scan:ColorMode><scan:ColorMode>Grayscale8</scan:ColorMode></scan:ColorModes>\
                 <scan:ContentTypes><pwg:ContentType>Photo</pwg:ContentType><pwg:ContentType>Text</pwg:ContentType><pwg:ContentType>TextAndPhoto</pwg:ContentType></scan:ContentTypes>\
                 <scan:DocumentFormats><pwg:DocumentFormat>application/pdf</pwg:DocumentFormat><pwg:DocumentFormat>image/jpeg</pwg:DocumentFormat>\
                 <scan:DocumentFormatExt>application/pdf</scan:DocumentFormatExt><scan:DocumentFormatExt>image/jpeg</scan:DocumentFormatExt></scan:DocumentFormats>\
                 <scan:SupportedResolutions><scan:DiscreteResolutions>{res}</scan:DiscreteResolutions></scan:SupportedResolutions>\
                 <scan:ColorSpaces><scan:ColorSpace>sRGB</scan:ColorSpace></scan:ColorSpaces>\
                 </scan:SettingProfile></scan:SettingProfiles>\
                 <scan:SupportedIntents><scan:Intent>Document</scan:Intent><scan:Intent>TextAndGraphic</scan:Intent>\
                 <scan:Intent>Photo</scan:Intent><scan:Intent>Preview</scan:Intent></scan:SupportedIntents>\
                 <scan:MaxOpticalXResolution>{max_dpi}</scan:MaxOpticalXResolution><scan:MaxOpticalYResolution>{max_dpi}</scan:MaxOpticalYResolution>",
                caps.max_width / 4,
                max_len / 4
            )
        };
        let adf = if caps.has_adf {
            format!(
                "<scan:Adf><scan:AdfSimplexInputCaps>{}</scan:AdfSimplexInputCaps>\
                 <scan:FeederCapacity>40</scan:FeederCapacity><scan:AdfOptions/></scan:Adf>",
                input(caps.max_len_adf)
            )
        } else {
            String::new()
        };
        let admin = self.admin_url.as_deref().map(|u| format!("<scan:AdminURI>{}</scan:AdminURI>", escape(u))).unwrap_or_default();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<scan:ScannerCapabilities {NS}><pwg:Version>{VERSION}</pwg:Version><pwg:MakeAndModel>{} {}</pwg:MakeAndModel><scan:UUID>{}</scan:UUID>{admin}<scan:Platen><scan:PlatenInputCaps>{}</scan:PlatenInputCaps></scan:Platen>{adf}</scan:ScannerCapabilities>"#,
            escape(&caps.vendor),
            escape(&caps.model),
            self.uuid,
            input(caps.max_len_flatbed)
        )
    }

    fn status_xml(&self) -> String {
        let caps = self.caps();
        let jobs = self.jobs.lock().expect("jobs lock");
        let busy = jobs.values().any(|j| j.state == JobState::Processing);
        let adf = if caps.has_adf {
            format!("<scan:AdfState>{}</scan:AdfState>", if caps.adf_loaded { "ScannerAdfLoaded" } else { "ScannerAdfEmpty" })
        } else {
            String::new()
        };
        let mut list: Vec<(&String, &Job)> = jobs.iter().collect();
        list.sort_by_key(|(_, j)| std::cmp::Reverse(j.created));
        let jobs_xml: String = list
            .iter()
            .map(|(id, j)| {
                format!(
                    "<scan:JobInfo><pwg:JobUri>/eSCL/ScanJobs/{id}</pwg:JobUri><pwg:JobUuid>{id}</pwg:JobUuid>\
                     <scan:Age>{}</scan:Age><pwg:ImagesCompleted>{}</pwg:ImagesCompleted><pwg:ImagesToTransfer>{}</pwg:ImagesToTransfer>\
                     <pwg:JobState>{:?}</pwg:JobState><pwg:JobStateReasons><pwg:JobStateReason>{}</pwg:JobStateReason></pwg:JobStateReasons></scan:JobInfo>",
                    j.created.elapsed().as_secs(),
                    j.images_completed,
                    j.pages.len(),
                    j.state,
                    j.reason
                )
            })
            .collect();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<scan:ScannerStatus {NS}><pwg:Version>{VERSION}</pwg:Version><pwg:State>{}</pwg:State>{adf}<scan:Jobs>{jobs_xml}</scan:Jobs></scan:ScannerStatus>"#,
            if busy { "Processing" } else { "Idle" }
        )
    }

    /// TXT record for `_uscan._tcp`.
    pub fn txt_record(&self) -> Vec<(String, String)> {
        let caps = self.caps();
        let mut txt = vec![
            ("txtvers", "1".to_string()),
            ("vers", VERSION.to_string()),
            ("rs", "eSCL".to_string()),
            ("ty", format!("{} {}", caps.vendor, caps.model)),
            ("pdl", "application/pdf,image/jpeg".to_string()),
            ("UUID", self.uuid.clone()),
            ("cs", "color,grayscale".to_string()),
            ("is", if caps.has_adf { "platen,adf" } else { "platen" }.to_string()),
            ("duplex", "F".to_string()),
        ];
        if let Some(url) = &self.admin_url {
            txt.push(("adminurl", url.clone()));
        }
        txt.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }
}

/// Reads the scan settings a client posted; anything missing or odd falls back to sane defaults.
fn parse_settings(xml: &str, caps: &Capabilities) -> (ScanParams, bool) {
    let num = |tag: &str| xml_value(xml, tag).and_then(|v| v.parse::<u32>().ok());
    let offered: Vec<u32> = RESOLUTIONS.iter().copied().filter(|r| caps.resolutions.contains(r)).collect();
    let wanted = num("XResolution").unwrap_or(300);
    let dpi = offered.iter().copied().min_by_key(|r| r.abs_diff(wanted)).unwrap_or(300);
    let color = !matches!(xml_value(xml, "ColorMode"), Some("Grayscale8" | "Grayscale16" | "BlackAndWhite1"));
    let adf = xml_value(xml, "InputSource") == Some("Feeder") && caps.has_adf;
    let format = xml_value(xml, "DocumentFormatExt").or_else(|| xml_value(xml, "DocumentFormat")).unwrap_or("image/jpeg");
    let pdf = format.contains("pdf");

    let max_len = if adf { caps.max_len_adf } else { caps.max_len_flatbed };
    let x = (num("XOffset").unwrap_or(0) * 4).min(caps.max_width.saturating_sub(64));
    let y = (num("YOffset").unwrap_or(0) * 4).min(max_len.saturating_sub(64));
    let width = num("Width").map_or(caps.max_width, |w| w * 4).clamp(64, caps.max_width - x);
    let height = num("Height").map_or(max_len, |h| h * 4).clamp(64, max_len - y);
    (ScanParams { dpi, color, adf, x, y, width, height }, pdf)
}

/// Text of the first element with this local name (namespace prefix ignored).
fn xml_value<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        let end = rest.find('>')?;
        let tag = rest[..end].split_whitespace().next().unwrap_or("");
        rest = &rest[end + 1..];
        if !tag.starts_with('/') && tag.rsplit(':').next() == Some(name) {
            return rest.find('<').map(|close| rest[..close].trim());
        }
    }
    None
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn xml(code: StatusCode, body: String) -> Response<Body> {
    let mut res = Response::new(full(body));
    *res.status_mut() = code;
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/xml"));
    res
}

fn status(code: StatusCode) -> Response<Body> {
    let mut res = Response::new(full(""));
    *res.status_mut() = code;
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> Capabilities {
        Capabilities {
            vendor: "Samsung".into(),
            model: "M267x 287x Series".into(),
            resolutions: vec![75, 100, 150, 200, 300, 600],
            max_width: 10208,
            max_len_flatbed: 14040,
            max_len_adf: 16800,
            has_adf: true,
            adf_loaded: false,
            jpeg: true,
            line_order: true,
        }
    }

    #[test]
    fn parses_apple_style_settings() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<scan:ScanSettings xmlns:scan="http://schemas.hp.com/imaging/escl/2011/05/03" xmlns:pwg="http://www.pwg.org/schemas/2010/12/sm">
  <pwg:Version>2.6</pwg:Version><scan:Intent>Document</scan:Intent>
  <pwg:ScanRegions><pwg:ScanRegion><pwg:Height>3300</pwg:Height><pwg:ContentRegionUnits>escl:ThreeHundredthsOfInches</pwg:ContentRegionUnits>
  <pwg:Width>2550</pwg:Width><pwg:XOffset>0</pwg:XOffset><pwg:YOffset>0</pwg:YOffset></pwg:ScanRegion></pwg:ScanRegions>
  <pwg:InputSource>Platen</pwg:InputSource><scan:ColorMode>Grayscale8</scan:ColorMode>
  <scan:XResolution>300</scan:XResolution><scan:YResolution>300</scan:YResolution>
  <pwg:DocumentFormat>application/octet-stream</pwg:DocumentFormat><scan:DocumentFormatExt>application/pdf</scan:DocumentFormatExt>
</scan:ScanSettings>"#;
        let (p, pdf) = parse_settings(xml, &caps());
        assert_eq!((p.dpi, p.color, p.adf, pdf), (300, false, false, true));
        assert_eq!((p.x, p.y, p.width, p.height), (0, 0, 10200, 13200));
    }

    #[test]
    fn defaults_and_clamping() {
        let (p, pdf) = parse_settings("<ScanSettings><XResolution>250</XResolution><Width>99999</Width></ScanSettings>", &caps());
        assert_eq!((p.dpi, p.color, pdf), (200, true, false));
        assert_eq!(p.width, 10208);
        assert_eq!(p.height, 14040);
    }

    #[test]
    fn xml_value_ignores_prefix_and_closing_tags() {
        assert_eq!(xml_value("<a:X>1</a:X><Y>2</Y>", "Y"), Some("2"));
        assert_eq!(xml_value("<X>1</X>", "Z"), None);
    }
}
