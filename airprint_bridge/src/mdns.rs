//! Bonjour advertisement with the TXT record current iOS expects from an AirPrint printer.

use std::net::{IpAddr, ToSocketAddrs};

use mdns_sd::{IfKind, IfPredicate, ServiceDaemon, ServiceInfo, TxtProperty};

use crate::config::{Mode, Options};
use crate::printer::{PrinterInfo, Target};

/// `_ipp._tcp` plus the `_universal` subtype that marks a printer as AirPrint.
const SERVICE_TYPE: &str = "_universal._sub._ipp._tcp.local.";
pub const HOST_NAME: &str = "airprint-bridge.local.";
/// A TXT string holds at most 255 bytes, key and '=' included.
const TXT_MAX: usize = 255;

pub fn txt_record(target: &Target, info: &PrinterInfo, uuid: &str) -> Vec<(String, String)> {
    let flag = |b: bool| if b { "T" } else { "F" }.to_string();
    let mut txt = vec![
        ("txtvers", "1".to_string()),
        ("qtotal", "1".to_string()),
        ("rp", target.path.trim_start_matches('/').to_string()),
        ("ty", info.make_model.clone()),
        ("product", format!("({})", info.make_model)),
        ("note", info.location.clone()),
        ("pdl", pdl(&info.formats)),
        ("URF", info.urf.join(",")),
        ("UUID", uuid.to_string()),
        ("Color", flag(info.color)),
        ("Duplex", flag(info.duplex)),
        ("Copies", "T".to_string()),
        ("kind", "document".to_string()),
        ("priority", "0".to_string()),
        ("air", "none".to_string()),
    ];
    if let Some(url) = &info.admin_url {
        txt.push(("adminurl", url.clone()));
    }
    txt.into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .filter(|(k, v)| k.len() + 1 + v.len() <= TXT_MAX)
        .collect()
}

/// The printer's formats with `image/urf` first, trimmed to fit one TXT string.
fn pdl(formats: &[String]) -> String {
    let mut list = vec!["image/urf".to_string()];
    list.extend(formats.iter().filter(|f| f.as_str() != "image/urf").cloned());
    let mut out = String::new();
    for f in list {
        let next = if out.is_empty() { f } else { format!("{out},{f}") };
        if "pdl=".len() + next.len() > TXT_MAX {
            break;
        }
        out = next;
    }
    out
}

/// An mDNS daemon on the LAN interfaces only: no loopback, no container networks on the Home Assistant host.
pub fn daemon() -> Result<ServiceDaemon, String> {
    let daemon = ServiceDaemon::new().map_err(|e| format!("starting mDNS: {e}"))?;
    daemon
        .disable_interface(IfKind::Predicate(IfPredicate::new(|intf| {
            intf.is_loopback()
                || ["docker", "hassio", "veth", "br-"].iter().any(|p| intf.name.starts_with(p))
        })))
        .map_err(|e| format!("configuring mDNS interfaces: {e}"))?;
    Ok(daemon)
}

/// Publishes (or re-publishes, after the printer moved) the bridge's record.
pub fn register(daemon: &ServiceDaemon, opts: &Options, target: &Target, txt: &[(String, String)]) -> Result<(), String> {
    let props: Vec<TxtProperty> = txt.iter().map(|(k, v)| TxtProperty::from((k.as_str(), v.as_str()))).collect();
    let info = match opts.mode {
        Mode::Proxy => ServiceInfo::new(SERVICE_TYPE, &opts.name, HOST_NAME, (), opts.port, props)
            .map(ServiceInfo::enable_addr_auto),
        Mode::AdvertiseOnly => {
            let ip = resolve(&target.host)?;
            ServiceInfo::new(SERVICE_TYPE, &opts.name, HOST_NAME, ip, target.port, props)
        }
    }
    .map_err(|e| format!("building mDNS record: {e}"))?;
    daemon.register(info).map_err(|e| format!("registering mDNS record: {e}"))
}

/// Publishes the AirScan (eSCL) record for the bridge's scan server.
pub fn register_scanner(daemon: &ServiceDaemon, opts: &Options, txt: &[(String, String)]) -> Result<(), String> {
    let props: Vec<TxtProperty> = txt.iter().map(|(k, v)| TxtProperty::from((k.as_str(), v.as_str()))).collect();
    let info = ServiceInfo::new("_uscan._tcp.local.", &opts.name, HOST_NAME, (), opts.scan_port, props)
        .map(ServiceInfo::enable_addr_auto)
        .map_err(|e| format!("building mDNS scanner record: {e}"))?;
    daemon.register(info).map_err(|e| format!("registering mDNS scanner record: {e}"))
}

fn resolve(host: &str) -> Result<IpAddr, String> {
    if let Ok(ip) = host.parse() {
        return Ok(ip);
    }
    (host, 0)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next())
        .map(|a| a.ip())
        .ok_or_else(|| format!("cannot resolve printer_host {host:?}; use its IP address"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdl_puts_urf_first_and_fits() {
        let formats: Vec<String> = (0..40).map(|i| format!("application/x-format-{i}")).collect();
        let p = pdl(&formats);
        assert!(p.starts_with("image/urf,application/x-format-0"));
        assert!(p.len() + 4 <= TXT_MAX);
    }

    #[test]
    fn pdl_does_not_duplicate_urf() {
        let p = pdl(&["application/PCL".into(), "image/urf".into()]);
        assert_eq!(p, "image/urf,application/PCL");
    }
}
