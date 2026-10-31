//! Samsung/Xerox network scan protocol (TCP port 9400), as spoken by SANE's xerox_mfp backend.
//!
//! Every command is `1b a8 <cmd> <len> …`; replies are `a8 <status> <len> <msg> …`.
//! Window sizes are in 1/1200 inch.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const CMD_ABORT: u8 = 0x06;
const CMD_INQUIRY: u8 = 0x12;
const CMD_RESERVE_UNIT: u8 = 0x16;
const CMD_RELEASE_UNIT: u8 = 0x17;
const CMD_SET_WINDOW: u8 = 0x24;
const CMD_READ: u8 = 0x28;
const CMD_READ_IMAGE: u8 = 0x29;
const CMD_OBJECT_POSITION: u8 = 0x31;

const RES_CODE: u8 = 0xa8;
const STATUS_CHECK: u8 = 0x02;
const STATUS_CANCEL: u8 = 0x04;
const STATUS_BUSY: u8 = 0x08;
const MSG_PRODUCT_INFO: u8 = 0x10;
const MSG_SCANNING_PARAM: u8 = 0x30;
const MSG_END_BLOCK: u8 = 0x81;

const STATE_NO_DOCUMENT: u16 = 0x010;
const STATE_DOCUMENT_JAM: u16 = 0x020;
const STATE_COVER_OPEN: u16 = 0x040;
const STATE_WARMING: u16 = 0x080;
const STATE_LOCKING: u16 = 0x100;
const STATE_INVALID_AREA: u16 = 0x200;
const STATE_RESOURCE_BUSY: u16 = 0x400;

/// TCP port of the scan service.
pub const PORT: u16 = 9400;

pub const MODE_GRAY8: u8 = 0x03;
pub const MODE_RGB24: u8 = 0x05;
pub const DOC_ADF: u8 = 0x20;
pub const DOC_FLATBED: u8 = 0x40;

/// Compression type bit for JPEG (lossy) colour scans.
const COMPRESSION_JPEG: u8 = 1 << 6;

const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Resolution code sent in SET_WINDOW, by dpi.
const RES_DPI_CODES: [u32; 14] = [75, 0, 150, 0, 0, 300, 0, 600, 1200, 200, 100, 2400, 4800, 9600];
/// Bits of the INQUIRY resolution mask, by dpi.
const INQ_DPI_BITS: [u32; 20] = [75, 150, 0, 0, 200, 300, 0, 0, 600, 0, 0, 1200, 100, 0, 0, 2400, 0, 4800, 0, 9600];

#[derive(Debug, PartialEq)]
pub enum ScanError {
    NoDocument,
    Jammed,
    CoverOpen,
    Busy,
    Cancelled,
    Io(String),
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::NoDocument => write!(f, "no document"),
            ScanError::Jammed => write!(f, "document jammed"),
            ScanError::CoverOpen => write!(f, "cover open"),
            ScanError::Busy => write!(f, "scanner busy"),
            ScanError::Cancelled => write!(f, "cancelled"),
            ScanError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ScanError {}

impl From<std::io::Error> for ScanError {
    fn from(e: std::io::Error) -> Self {
        ScanError::Io(e.to_string())
    }
}

impl From<tokio::time::error::Elapsed> for ScanError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        ScanError::Io("scanner timed out".into())
    }
}

/// What the scanner reports about itself.
#[derive(Clone, Debug)]
pub struct Capabilities {
    pub vendor: String,
    pub model: String,
    pub resolutions: Vec<u32>,
    /// Max window in 1/1200 inch.
    pub max_width: u32,
    pub max_len_flatbed: u32,
    pub max_len_adf: u32,
    pub has_adf: bool,
    pub adf_loaded: bool,
    pub jpeg: bool,
    /// Colour data comes as separate R, G and B runs per line.
    pub line_order: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct ScanParams {
    pub dpi: u32,
    pub color: bool,
    pub adf: bool,
    /// Window in 1/1200 inch.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// One scanned page.
pub enum Page {
    /// Colour scans arrive as one baseline JPEG per block (a strip of lines).
    Jpeg(Vec<Vec<u8>>),
    /// 8-bit samples, interleaved RGB if `channels == 3`.
    Raw { width: u32, height: u32, channels: u8, data: Vec<u8> },
}

pub struct Scanner {
    stream: TcpStream,
    reserved: bool,
}

impl Scanner {
    pub async fn connect(host: &str, port: u16) -> Result<Self, ScanError> {
        let stream = timeout(Duration::from_secs(10), TcpStream::connect((host, port))).await??;
        stream.set_nodelay(true)?;
        Ok(Scanner { stream, reserved: false })
    }

    /// Sends a command and reads its fixed-size reply.
    async fn command(&mut self, cmd: &[u8], reply_len: usize) -> Result<Vec<u8>, ScanError> {
        let debug = std::env::var_os("SCAN_DEBUG").is_some();
        if debug {
            eprintln!("[debug] -> {cmd:02x?}");
        }
        timeout(IO_TIMEOUT, self.stream.write_all(cmd)).await??;
        let mut reply = vec![0u8; reply_len];
        let mut got = 0;
        while got < reply_len {
            match timeout(IO_TIMEOUT, self.stream.read(&mut reply[got..])).await {
                Ok(Ok(0)) => return Err(ScanError::Io("scanner closed the connection".into())),
                Ok(Ok(n)) => got += n,
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {
                    if debug {
                        eprintln!("[debug] <- timeout after {got} bytes: {:02x?}", &reply[..got]);
                    }
                    return Err(ScanError::Io("scanner timed out".into()));
                }
            }
        }
        if debug {
            eprintln!("[debug] <- {:02x?}", &reply[..reply_len.min(16)]);
        }
        if reply[0] != RES_CODE {
            return Err(ScanError::Io(format!("bad reply header {:02x}", reply[0])));
        }
        Ok(reply)
    }

    async fn simple(&mut self, command: u8) -> Result<Vec<u8>, ScanError> {
        let len = if command == CMD_INQUIRY { 70 } else { 32 };
        self.command(&[0x1b, 0xa8, command, 0x00], len).await
    }

    /// Runs a command, retrying while the scanner reports busy (warming up, moving).
    async fn wait(&mut self, command: u8, cmd: &[u8]) -> Result<Vec<u8>, ScanError> {
        let mut delay = Duration::from_millis(10);
        let start = tokio::time::Instant::now();
        loop {
            let reply = if cmd.is_empty() { self.simple(command).await? } else { self.command(cmd, 32).await? };
            match check(command, &reply) {
                Err(ScanError::Busy) if start.elapsed() < Duration::from_secs(120) => {
                    sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(1));
                }
                other => return other.map(|_| reply),
            }
        }
    }

    pub async fn inquiry(&mut self) -> Result<Capabilities, ScanError> {
        let r = self.simple(CMD_INQUIRY).await?;
        if r[3] != MSG_PRODUCT_INFO {
            return Err(ScanError::Io(format!("bad INQUIRY reply {:02x}", r[3])));
        }
        let name = String::from_utf8_lossy(&r[4..0x24]).trim_matches(|c: char| c == '\0' || c == ' ').to_string();
        let (vendor, model) = name.split_once(' ').map_or((name.clone(), String::new()), |(v, m)| (v.into(), m.trim().into()));
        let u32_at = |i: usize| u32::from_be_bytes([r[i], r[i + 1], r[i + 2], r[i + 3]]);
        let mask = (r[0x37] as u32) << 16 | (r[0x24] as u32) << 8 | r[0x25] as u32;
        let mut resolutions: Vec<u32> =
            INQ_DPI_BITS.iter().enumerate().filter(|(i, d)| **d != 0 && mask & (1 << i) != 0).map(|(_, d)| *d).collect();
        resolutions.sort();
        let has_adf = r[0x26] & 0x03 != 0;
        Ok(Capabilities {
            vendor,
            model,
            resolutions,
            max_width: u32_at(0x28),
            max_len_adf: u32_at(0x38),
            max_len_flatbed: u32_at(0x3c),
            has_adf,
            adf_loaded: has_adf && r[0x35] == 0x02,
            jpeg: r[0x32] & COMPRESSION_JPEG != 0,
            line_order: r[0x31] != 0,
        })
    }

    /// Reserves the unit and configures the scan window.
    pub async fn start(&mut self, p: &ScanParams, jpeg: bool) -> Result<(), ScanError> {
        self.wait(CMD_RESERVE_UNIT, &[]).await?;
        self.reserved = true;

        let res = RES_DPI_CODES.iter().position(|d| *d == p.dpi).unwrap_or(0) as u8;
        let inch = |v: u32| -> (u8, u8) {
            // Offsets are sent as whole inches and hundredths.
            let hundredths = v * 100 / 1200;
            ((hundredths / 100) as u8, (hundredths % 100) as u8)
        };
        let (xi, xf) = inch(p.x);
        let (yi, yf) = inch(p.y);
        let mut cmd = [0u8; 0x19];
        cmd[..5].copy_from_slice(&[0x1b, 0xa8, CMD_SET_WINDOW, 0x13, MSG_SCANNING_PARAM]);
        cmd[0x05..0x09].copy_from_slice(&p.width.to_be_bytes());
        cmd[0x09..0x0d].copy_from_slice(&p.height.to_be_bytes());
        cmd[0x0d] = res;
        cmd[0x0e] = res;
        cmd[0x0f] = xi;
        cmd[0x10] = xf;
        cmd[0x11] = yi;
        cmd[0x12] = yf;
        cmd[0x13] = if p.color { MODE_RGB24 } else { MODE_GRAY8 };
        if p.color && jpeg {
            cmd[0x14] = 0x06;
        }
        cmd[0x16] = 2; // threshold (lineart only)
        cmd[0x17] = if p.adf { DOC_ADF } else { DOC_FLATBED };
        self.wait(CMD_SET_WINDOW, &cmd).await?;
        Ok(())
    }

    /// Positions the next sheet and reads one page. `NoDocument` once the feeder is empty.
    pub async fn read_page(&mut self, p: &ScanParams, caps: &Capabilities, jpeg: bool) -> Result<Page, ScanError> {
        self.wait(CMD_OBJECT_POSITION, &[]).await?;
        let channels: u8 = if p.color { 3 } else { 1 };
        let mut data = Vec::new();
        let mut strips = Vec::new();
        let mut width = 0u32;
        let mut lines = 0u32;
        loop {
            let r = self.wait(CMD_READ, &[]).await?;
            let block_len = u32::from_be_bytes([r[4], r[5], r[6], r[7]]) as usize;
            let vertical = u16::from_be_bytes([r[8], r[9]]) as u32;
            let horizontal = u16::from_be_bytes([r[10], r[11]]) as u32;
            let last = r[3] == MSG_END_BLOCK;
            if std::env::var_os("SCAN_DEBUG").is_some() {
                eprintln!("[debug] READ reply {:02x?} -> block {block_len} v {vertical} h {horizontal} last {last}", &r[..16]);
            }

            timeout(IO_TIMEOUT, self.stream.write_all(&[0x1b, 0xa8, CMD_READ_IMAGE, 0x00])).await??;
            let mut block = vec![0u8; block_len];
            timeout(IO_TIMEOUT, self.stream.read_exact(&mut block)).await??;

            if p.color && jpeg {
                strips.push(block);
            } else {
                width = horizontal;
                let line_bytes = (horizontal * channels as u32) as usize;
                for line in block.chunks_exact(line_bytes).take(vertical as usize) {
                    if channels == 3 && caps.line_order {
                        // RRR…GGG…BBB → RGBRGB…
                        let w = horizontal as usize;
                        for x in 0..w {
                            data.extend_from_slice(&[line[x], line[w + x], line[2 * w + x]]);
                        }
                    } else {
                        data.extend_from_slice(line);
                    }
                }
                lines += vertical;
            }
            if last {
                break;
            }
        }
        if p.color && jpeg {
            return Ok(Page::Jpeg(strips));
        }
        // Crop the scanner's padding to the requested window.
        let want_w = (p.width as u64 * p.dpi as u64 / 1200) as u32;
        let want_h = (p.height as u64 * p.dpi as u64 / 1200) as u32;
        let (out_w, out_h) = (want_w.min(width).max(1), want_h.min(lines).max(1));
        let mut out = Vec::with_capacity((out_w * out_h * channels as u32) as usize);
        let stride = (width * channels as u32) as usize;
        for row in data.chunks_exact(stride).take(out_h as usize) {
            out.extend_from_slice(&row[..(out_w * channels as u32) as usize]);
        }
        Ok(Page::Raw { width: out_w, height: out_h, channels, data: out })
    }

    pub async fn stop(&mut self) {
        if self.reserved {
            self.reserved = false;
            let _ = self.simple(CMD_RELEASE_UNIT).await;
        }
    }

    pub async fn abort(&mut self) {
        let _ = self.simple(CMD_ABORT).await;
        self.stop().await;
    }
}

/// Maps a reply's status to an error, like xerox_mfp's dev_command.
fn check(command: u8, r: &[u8]) -> Result<(), ScanError> {
    if !matches!(command, CMD_SET_WINDOW | CMD_OBJECT_POSITION | CMD_READ | CMD_RESERVE_UNIT) {
        return Ok(());
    }
    match r[1] {
        STATUS_BUSY => Err(ScanError::Busy),
        STATUS_CANCEL => Err(ScanError::Cancelled),
        STATUS_CHECK => {
            let state = if command == CMD_READ {
                u16::from_be_bytes([r[12], r[13]])
            } else {
                u16::from_be_bytes([r[4], r[5]])
            };
            match state {
                s if s & STATE_DOCUMENT_JAM != 0 => Err(ScanError::Jammed),
                s if s & STATE_NO_DOCUMENT != 0 => Err(ScanError::NoDocument),
                s if s & STATE_COVER_OPEN != 0 => Err(ScanError::CoverOpen),
                s if s & STATE_INVALID_AREA != 0 => Err(ScanError::Io("invalid scan area".into())),
                s if s & (STATE_WARMING | STATE_RESOURCE_BUSY) != 0 => Err(ScanError::Busy),
                s if s & STATE_LOCKING != 0 => Err(ScanError::Io("scanner lock engaged".into())),
                s if s & !0x001 != 0 => Err(ScanError::Busy),
                _ => Ok(()),
            }
        }
        _ => Ok(()),
    }
}

/// Scans with a fresh connection and sends each page as soon as it is read: one page from
/// the flatbed, or until the feeder is empty. Dropping the receiver cancels the scan.
pub async fn scan(host: &str, port: u16, params: ScanParams, pages: mpsc::Sender<Page>) -> Result<(), ScanError> {
    let mut scanner = Scanner::connect(host, port).await?;
    let caps = scanner.inquiry().await?;
    // Colour only works with the scanner's JPEG compression; raw colour stalls the M267x.
    let jpeg = caps.jpeg;
    let result = async {
        scanner.start(&params, jpeg).await?;
        let mut count = 0;
        loop {
            match scanner.read_page(&params, &caps, jpeg).await {
                Ok(page) => {
                    count += 1;
                    if pages.send(page).await.is_err() {
                        return Err(ScanError::Cancelled);
                    }
                }
                Err(ScanError::NoDocument) if count > 0 => return Ok(()),
                Err(e) => return Err(e),
            }
            if !params.adf {
                return Ok(());
            }
        }
    }
    .await;
    match result {
        Ok(()) => scanner.stop().await,
        Err(_) => scanner.abort().await,
    }
    result
}

/// Asks the scanner what it can do.
pub async fn capabilities(host: &str, port: u16) -> Result<Capabilities, ScanError> {
    Scanner::connect(host, port).await?.inquiry().await
}
