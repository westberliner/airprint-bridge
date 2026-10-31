# AirPrint Bridge for Home Assistant

A Home Assistant app (formerly "add-on") that makes older AirPrint printers visible to current iOS again.
It finds the printer via Bonjour, advertises a clean record for it, and optionally runs an IPP pass-through
proxy. It also offers Samsung multifunction scanners (network scan on port 9400) as AirScan (eSCL) scanners
for macOS. All of this runs from one small Rust binary. Configuration and troubleshooting: [airprint_bridge/DOCS.md](airprint_bridge/DOCS.md).

## Install as a local app (no GitHub needed)

For a 64-bit Raspberry Pi (aarch64). Run `make dist` on a Mac or PC with Docker first.

1. **Make the device's folders reachable** (one-time): Settings → Apps → App store → install the
   **Samba share** app, set a username and password in its Configuration tab, and start it.
2. **Copy the app**: on the Mac, Finder → ⌘K → `smb://homeassistant.local`, log in, open the
   `addons` share (on newer versions possibly `apps`), and copy the `airprint_bridge` folder into it,
   so you get `addons/airprint_bridge/config.yaml`. Copy the folder from `dist/`, made by `make dist`:
   it holds a binary already compiled for aarch64, so the device doesn't compile Rust itself.
3. **Install**: Settings → Apps → App store → ⋮ → **Check for updates**, reload, then under
   **Local apps** pick **AirPrint Bridge** → Install. That takes seconds.
4. **Configure and start**: in Configuration, set **Printer name** (shown on the iPhone) and leave the rest empty.
   In Info, turn on **Start on boot** and **Watchdog**, then **Start**. The log should show
   `found printer …`, `advertising … via Bonjour`, and `IPP proxy listening on port 631`.
5. **Print**: on the iPhone (same Wi-Fi), Share → Print → pick the printer.

**Updating:** copy the folder again (with a higher `version` in `config.yaml`), run **Check for updates**,
then **Update** or **Rebuild** on the app page.

**Troubleshooting the install:** if "Local apps" doesn't show up, check that `config.yaml` sits directly
in `addons/airprint_bridge/` and not one folder deeper. Build errors appear under Settings → System → Logs → Supervisor.

## Install from a GitHub repository

Push this repository to GitHub and replace `CHANGE_ME` in `repository.yaml` and `airprint_bridge/config.yaml`.
Then in Home Assistant go to Settings → Apps → App store → ⋮ → Repositories, add the repository URL, and install
**AirPrint Bridge**. To skip compiling on the device, run the GitHub workflow in `.github/workflows/build.yml`
once and uncomment `image:` in `config.yaml`.

## Development

```sh
cd airprint_bridge
cargo test
cargo run -- options.json   # same format as /data/options.json, e.g. {"name": "Test", "port": 8631}
cargo run -- scan-test 192.168.178.200 out.pdf 300 color   # one scan straight from the scanner
SCAN_DEBUG=1 cargo run -- scan-test …                      # trace the scanner protocol
make -C .. dist             # prebuilt aarch64 app for a local install in dist/airprint_bridge
```

## License

MIT, see [LICENSE](LICENSE). This software is based in part on the work of the Independent JPEG Group
(via the `jpeg-encoder` crate).
