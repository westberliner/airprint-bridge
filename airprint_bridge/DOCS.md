# AirPrint Bridge

Makes an older AirPrint printer show up on current iPhones and iPads again.

## Why the printer disappeared

iOS only lists printers whose Bonjour record (`_ipp._tcp` with the `_universal` subtype) carries
a valid AirPrint TXT record. Older printers often advertise a broken or outdated one. The Samsung M267x/287x
series, for example, sends a corrupted `PaperCustom` entry, and current iOS ignores the whole printer.

This add-on publishes a clean record for the printer. In `proxy` mode it also relays print jobs and
fixes the attributes the printer reports (IPP 2.0, `urf-supported`, `printer-uuid`, URIs). Jobs are
streamed through unchanged, so the printer must already accept `image/urf` (Apple Raster),
and every AirPrint printer does. No CUPS and no drivers: the add-on is a single 2 MB binary.

## Scanning (AirScan)

If the printer is a multifunction device with Samsung's network scan service (TCP port 9400,
as on the M267x/287x series), the add-on also offers its scanner as an **AirScan (eSCL)** scanner.
On a Mac, open **Image Capture**, or in **Preview** choose File → Import from Scanner. With the same
UUID as the printer, macOS also shows a **Scan** tab under System Settings → Printers & Scanners.
iPhone and iPad have no built-in network scanning; use an app that supports AirScan/eSCL.

- Flatbed and document feeder, color and grayscale, 75–600 dpi, JPEG or PDF.
- Color arrives from the scanner as JPEG strips, which the add-on joins and re-encodes once.
- One scan at a time; a second client gets "busy" until the first finishes.

## Setup

1. Set a **Printer name** (shown on the iPhone) and start the add-on. The printer is found
   automatically and followed when its IP changes. With several printers on the network, set
   **Printer name match** (`printer_match`) to part of the right one's name.
2. Optionally set **Printer IP address** (`printer_host`) to skip discovery, but then give the printer a fixed IP in your router.
3. On the iPhone (same Wi-Fi/subnet as Home Assistant): Share → Print → Select Printer.

Check the add-on log: it prints the TXT record it advertises.

## Options

| Option | Default | |
|---|---|---|
| `printer_host` | – | Printer IP address; empty = find it via Bonjour |
| `printer_match` | – | Part of the printer's Bonjour name, if discovery finds several printers |
| `printer_port` | `631` | Printer IPP port (manual IP only) |
| `printer_path` | `/ipp/print` | Printer IPP path (manual IP, or no `rp` in the printer's record) |
| `name` | `AirPrint Bridge` | Name shown on the iPhone |
| `mode` | `proxy` | `proxy` relays jobs through Home Assistant; `advertise_only` only publishes a record pointing at the printer |
| `port` | `631` | Port the proxy listens on |
| `color`, `duplex`, `urf` | from printer | Overrides for the advertised capabilities |
| `scanner` | `true` | Offer the scanner via AirScan (turns itself off if the printer has none) |
| `scan_port` | `8090` | Port of the AirScan (eSCL) server |

## Troubleshooting

- **"found several printers" in the log:** set `printer_match` to part of one of the listed names.
- **Printer not listed:** phone and Home Assistant must be on the same subnet/VLAN; mDNS doesn't cross routers.
- **Two entries, one broken:** turn off AirPrint/Bonjour in the printer's web interface, so only the bridge's record remains.
- **Listed but printing fails in `advertise_only`:** switch to `proxy`.
- **Scanner not listed on the Mac:** the log must show `advertising "…" as AirScan scanner`. A
  `no scanner at …` warning means the printer's scan service didn't answer on port 9400.

## License

MIT. This software is based in part on the work of the Independent JPEG Group (via the `jpeg-encoder` crate).
