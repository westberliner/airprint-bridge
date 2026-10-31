use serde::Deserialize;

use crate::printer::Target;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Advertise this host and relay IPP to the printer, fixing up its attributes.
    #[default]
    Proxy,
    /// Only advertise a clean Bonjour record that points straight at the printer.
    AdvertiseOnly,
}

/// Add-on options, as Home Assistant writes them to /data/options.json.
#[derive(Clone, Debug, Deserialize)]
pub struct Options {
    /// Printer IP; when unset the printer is found via Bonjour and followed when its IP changes.
    pub printer_host: Option<String>,
    /// Part of the printer's Bonjour name, to pick one when discovery finds several printers.
    pub printer_match: Option<String>,
    #[serde(default = "default_ipp_port")]
    pub printer_port: u16,
    #[serde(default = "default_printer_path")]
    pub printer_path: String,
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default = "default_ipp_port")]
    pub port: u16,
    pub color: Option<bool>,
    pub duplex: Option<bool>,
    /// Comma-separated URF capabilities, e.g. "W8,RS600,CP1,DM1,V1.4".
    pub urf: Option<String>,
    /// Offer the printer's scanner via AirScan (eSCL).
    #[serde(default = "default_true")]
    pub scanner: bool,
    #[serde(default = "default_scan_port")]
    pub scan_port: u16,
}

fn default_true() -> bool {
    true
}

fn default_scan_port() -> u16 {
    8090
}

fn default_ipp_port() -> u16 {
    631
}

fn default_printer_path() -> String {
    "/ipp/print".into()
}

fn default_name() -> String {
    "AirPrint Bridge".into()
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

impl Options {
    pub fn load(path: &str) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
        let mut opts: Options = serde_json::from_str(&raw).map_err(|e| format!("parsing {path}: {e}"))?;
        if !opts.printer_path.starts_with('/') {
            opts.printer_path.insert(0, '/');
        }
        opts.printer_host = non_empty(opts.printer_host);
        opts.printer_match = non_empty(opts.printer_match);
        opts.urf = non_empty(opts.urf);
        Ok(opts)
    }

    /// The printer given by IP in the options, if any.
    pub fn manual_target(&self) -> Option<Target> {
        self.printer_host.as_ref().map(|host| Target {
            host: host.clone(),
            port: self.printer_port,
            path: self.printer_path.clone(),
        })
    }

    pub fn urf_override(&self) -> Option<Vec<String>> {
        self.urf
            .as_ref()
            .map(|u| u.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
    }
}
