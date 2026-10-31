//! Finds the printer via Bonjour and follows it when its address changes.

use std::net::IpAddr;
use std::time::Duration;

use mdns_sd::{Receiver, ResolvedService, ServiceDaemon, ServiceEvent};
use tokio::time::{Instant, timeout_at};

use crate::config::Options;
use crate::mdns::HOST_NAME;
use crate::printer::{SharedTarget, Target};

const IPP_SERVICE: &str = "_ipp._tcp.local.";
/// After the first printer shows up, wait this long for others before picking the only one.
const SETTLE: Duration = Duration::from_secs(5);

pub struct Discovery {
    rx: Receiver<ServiceEvent>,
    fullname: String,
}

/// Browses `_ipp._tcp` until the printer is found: the one matching `printer_match`,
/// or the only printer on the network.
pub async fn find(daemon: &ServiceDaemon, opts: &Options) -> Result<(Discovery, Target), String> {
    let rx = daemon.browse(IPP_SERVICE).map_err(|e| format!("browsing for printers: {e}"))?;
    match &opts.printer_match {
        Some(m) => eprintln!("[info] looking for a printer whose name contains {m:?}"),
        None => eprintln!("[info] looking for printers on the network"),
    }
    let mut found: Vec<(String, Target)> = Vec::new();
    let mut settle_until: Option<Instant> = None;
    let mut ambiguous = false;
    loop {
        let event = match settle_until {
            Some(deadline) => timeout_at(deadline, rx.recv_async()).await.ok(),
            None => Some(rx.recv_async().await),
        };
        match event {
            Some(Ok(ServiceEvent::ServiceResolved(svc))) => {
                let Some(target) = candidate(&svc, opts) else { continue };
                let name = instance_name(&svc.fullname);
                if found.iter().all(|(f, _)| *f != svc.fullname) {
                    eprintln!("[info] found printer {name:?} at {}", target.uri());
                }
                if let Some(m) = &opts.printer_match {
                    if name.to_lowercase().contains(&m.to_lowercase()) {
                        return Ok((Discovery { rx, fullname: svc.fullname.clone() }, target));
                    }
                    continue;
                }
                // Queues a computer shares (CUPS names them "Printer @ Host") aren't the printer itself.
                if name.contains(" @ ") {
                    continue;
                }
                found.retain(|(f, _)| *f != svc.fullname);
                found.push((svc.fullname.clone(), target));
                if !ambiguous {
                    settle_until.get_or_insert_with(|| Instant::now() + SETTLE);
                }
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(format!("browsing for printers stopped: {e}")),
            None => {
                settle_until = None;
                if let [(fullname, target)] = found.as_slice() {
                    return Ok((Discovery { rx, fullname: fullname.clone() }, target.clone()));
                }
                ambiguous = true;
                let names: Vec<_> = found.iter().map(|(f, _)| instance_name(f)).collect();
                eprintln!("[warn] found several printers; set printer_match to part of one name: {names:?}");
            }
        }
    }
}

impl Discovery {
    pub fn name(&self) -> &str {
        instance_name(&self.fullname)
    }

    /// Keeps `shared` pointed at the printer and calls `on_change` whenever its address changes.
    pub async fn follow(self, opts: Options, shared: SharedTarget, on_change: impl Fn(&Target)) {
        while let Ok(event) = self.rx.recv_async().await {
            let ServiceEvent::ServiceResolved(svc) = event else { continue };
            if svc.fullname != self.fullname {
                continue;
            }
            let Some(target) = candidate(&svc, &opts) else { continue };
            let changed = {
                let mut current = shared.write().expect("target lock");
                let changed = *current != target;
                *current = target.clone();
                changed
            };
            if changed {
                eprintln!("[info] printer moved to {}", target.uri());
                on_change(&target);
            }
        }
    }
}

/// The printer's IPP endpoint, unless this is our own record or it has no usable address.
fn candidate(svc: &ResolvedService, opts: &Options) -> Option<Target> {
    if svc.host == HOST_NAME || instance_name(&svc.fullname) == opts.name {
        return None;
    }
    let ip = pick_address(svc.addresses.iter().map(|a| a.to_ip_addr()))?;
    let path = svc
        .get_property_val_str("rp")
        .map(|rp| format!("/{}", rp.trim_start_matches('/')))
        .unwrap_or_else(|| opts.printer_path.clone());
    Some(Target { host: ip.to_string(), port: svc.port, path })
}

/// Prefers IPv4; link-local IPv6 would need a scope id in the URI.
fn pick_address(addrs: impl Iterator<Item = IpAddr>) -> Option<IpAddr> {
    let mut usable: Vec<IpAddr> = addrs
        .filter(|ip| !ip.is_loopback())
        .filter(|ip| !matches!(ip, IpAddr::V6(v6) if v6.is_unicast_link_local()))
        .collect();
    usable.sort_by_key(|ip| (!ip.is_ipv4(), *ip));
    usable.into_iter().next()
}

fn instance_name(fullname: &str) -> &str {
    fullname.strip_suffix(&format!(".{IPP_SERVICE}")).unwrap_or(fullname)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_name_strips_service() {
        assert_eq!(instance_name("Samsung M267x._ipp._tcp.local."), "Samsung M267x");
    }

    #[test]
    fn prefers_ipv4_and_skips_link_local() {
        let addrs = ["fe80::1", "2001:db8::5", "192.168.178.200", "127.0.0.1"].map(|a| a.parse().unwrap());
        assert_eq!(pick_address(addrs.into_iter()), Some("192.168.178.200".parse().unwrap()));
        let v6 = ["fe80::1", "2001:db8::5"].map(|a| a.parse().unwrap());
        assert_eq!(pick_address(v6.into_iter()), Some("2001:db8::5".parse().unwrap()));
    }
}
