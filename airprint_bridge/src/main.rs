mod config;
mod discover;
mod escl;
mod image;
mod ipp;
mod mdns;
mod printer;
mod proxy;
mod scanner;

use std::process::ExitCode;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use config::{Mode, Options};
use printer::PrinterInfo;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[error] {e}");
            ExitCode::FAILURE
        }
    }
}

/// `airprint-bridge scan-test <host> <out.jpg|out.pdf> [dpi] [color|gray] [adf]`: one scan, for development.
async fn scan_test(args: &[String]) -> Result<(), String> {
    let [host, out, rest @ ..] = args else {
        return Err("usage: scan-test <host> <out.jpg|out.pdf> [dpi] [color|gray] [adf]".into());
    };
    let caps = scanner::capabilities(host, scanner::PORT).await.map_err(|e| e.to_string())?;
    eprintln!("{caps:#?}");
    let adf = rest.iter().any(|s| s == "adf");
    let params = scanner::ScanParams {
        dpi: rest.first().and_then(|d| d.parse().ok()).unwrap_or(150),
        color: rest.get(1).is_none_or(|m| m == "color"),
        adf,
        x: 0,
        y: 0,
        width: caps.max_width,
        height: if adf { caps.max_len_adf } else { caps.max_len_flatbed },
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let scan = tokio::spawn({
        let host = host.clone();
        async move { scanner::scan(&host, scanner::PORT, params, tx).await }
    });
    let mut i = 0;
    while let Some(page) = rx.recv().await {
        let doc = image::Document::from_page(page, &params, out.ends_with(".pdf"))?;
        let path = if i == 0 { out.clone() } else { format!("{out}.{i}") };
        eprintln!("page {i}: {} -> {path} ({} bytes)", doc.describe(), doc.bytes.len());
        std::fs::write(&path, &doc.bytes).map_err(|e| e.to_string())?;
        i += 1;
    }
    scan.await.map_err(|e| e.to_string())?.map_err(|e| e.to_string())
}

async fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "scan-test") {
        return scan_test(&args[1..]).await;
    }
    let path = args.first().cloned().unwrap_or_else(|| "/data/options.json".into());
    let opts = Options::load(&path)?;
    let daemon = mdns::daemon()?;

    let (target, discovery) = match opts.manual_target() {
        Some(target) => (target, None),
        None => {
            let (discovery, target) = discover::find(&daemon, &opts).await?;
            eprintln!("[info] using printer {:?}", discovery.name());
            (target, Some(discovery))
        }
    };
    eprintln!("[info] printer {} ({:?} mode)", target.uri(), opts.mode);

    let client = printer::client();
    let mut info = match printer::probe(&client, &target).await {
        Some(attrs) => PrinterInfo::from_attributes(&attrs, &opts),
        None => {
            eprintln!("[warn] printer not reachable; advertising with defaults and the add-on options");
            PrinterInfo::fallback(&opts)
        }
    };
    info.apply_overrides(&opts);
    eprintln!("[info] printer: {} (IPP {}.{})", info.make_model, info.max_version[0], info.max_version[1]);

    let uuid = printer::bridge_uuid(&opts.name);
    let txt = mdns::txt_record(&target, &info, &uuid);
    for (k, v) in &txt {
        eprintln!("[info]   TXT {k}={v}");
    }
    mdns::register(&daemon, &opts, &target, &txt)?;
    eprintln!("[info] advertising \"{}\" via Bonjour", opts.name);

    let shared = Arc::new(RwLock::new(target));

    let escl = if opts.scanner { start_scanner(&daemon, &opts, &shared, &uuid, &info).await } else { None };
    let scan_server = async {
        match escl {
            Some(escl) => escl.serve().await,
            None => std::future::pending().await,
        }
    };
    let server = async {
        match opts.mode {
            Mode::Proxy => {
                let proxy = proxy::Proxy {
                    opts: opts.clone(),
                    target: shared.clone(),
                    client,
                    uuid_urn: format!("urn:uuid:{uuid}"),
                    urf: info.urf.clone(),
                    max_version: info.max_version,
                };
                Arc::new(proxy).serve().await
            }
            Mode::AdvertiseOnly => std::future::pending().await,
        }
    };
    let follow = async {
        if let Some(discovery) = discovery {
            discovery
                .follow(opts.clone(), shared.clone(), |target| {
                    // The proxy reads the shared target; a direct record has to point at the new address.
                    if opts.mode == Mode::AdvertiseOnly {
                        let txt = mdns::txt_record(target, &info, &uuid);
                        if let Err(e) = mdns::register(&daemon, &opts, target, &txt) {
                            eprintln!("[warn] {e}");
                        }
                    }
                })
                .await;
        }
        std::future::pending::<()>().await
    };

    let result = tokio::select! {
        result = server => Err(format!("proxy stopped: {result:?}")),
        result = scan_server => Err(format!("scan server stopped: {result:?}")),
        _ = follow => unreachable!(),
        _ = shutdown_signal() => Ok(()),
    };

    // Sends the Bonjour goodbye so phones drop the printer right away.
    if let Ok(status) = daemon.shutdown() {
        let _ = status.recv_timeout(Duration::from_secs(2));
    }
    result
}

/// Asks the printer's scan service what it can do and, if it answers, advertises AirScan.
async fn start_scanner(
    daemon: &mdns_sd::ServiceDaemon,
    opts: &Options,
    target: &printer::SharedTarget,
    uuid: &str,
    info: &PrinterInfo,
) -> Option<Arc<escl::Escl>> {
    let host = target.read().expect("target lock").host.clone();
    let caps = match scanner::capabilities(&host, scanner::PORT).await {
        Ok(caps) => caps,
        Err(e) => {
            eprintln!("[warn] no scanner at {host}:{} ({e}); AirScan disabled", scanner::PORT);
            return None;
        }
    };
    eprintln!("[info] scanner: {} {} ({:?} dpi, feeder: {})", caps.vendor, caps.model, caps.resolutions, caps.has_adf);
    // Same UUID as the printer record, so macOS shows printer and scanner as one device.
    let escl = escl::Escl::new(uuid.to_string(), opts.scan_port, info.admin_url.clone(), target.clone(), caps);
    if let Err(e) = mdns::register_scanner(daemon, opts, &escl.txt_record()) {
        eprintln!("[warn] {e}");
        return None;
    }
    eprintln!("[info] advertising \"{}\" as AirScan scanner", opts.name);
    Some(Arc::new(escl))
}

async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing SIGTERM handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    eprintln!("[info] shutting down");
}
