# RCGUI

English | [简体中文](README.zh-CN.md)

RCGUI is a Windows desktop front end for [rclone](https://rclone.org/), written in Rust with a [Slint](https://slint.dev) UI.

It is a front end and nothing more: RCGUI never embeds rclone, never starts it, and never stops it. You run `rclone rcd` yourself — or let the bundled *one-click rclone* plugin start it for you — and RCGUI connects to that RC endpoint over HTTP. Everything the window shows is a projection of rclone's own state, so a change made from the command line appears here too. Mounts additionally need [WinFsp](https://winfsp.dev), which RCGUI can install for you with one click.

The interface ships in Chinese and English and switches at runtime.

> RCGUI is not the old *rcdeck* (the Node + Vue rewrite of the rclone web UI). It is a ground-up Rust/Slint application, and it does not ship a web front end at all — it only serves the plugin HTTP endpoints described below.

## Features

| Page | What you can do |
| --- | --- |
| **Dashboard** | Connection state and the RC endpoint actually in use, live counters for remotes / mounts / services / transfers, and a history of what the app just did |
| **Files** | Two-pane browser — local disk on the left, remote on the right — with a sort menu (name / size / modified), multi-select, upload to or download into a chosen folder, and drag-and-drop between the two panes |
| **Remotes** | Create, edit and delete remotes (name, backend type, `key=value` parameters), fill in a dedicated **K-Vault** form, keep a list of proxies and bind one per remote — re-applied on every start |
| **Mounts** | Mount any remote to a drive letter with cache mode and read-only flags, see whether the drive letter actually appeared, install WinFsp from the official or a mirror source — and have a mount you created re-created after the next successful connect |
| **Services** | Serve a remote over HTTP, WebDAV, FTP, SFTP or DLNA with per-service credentials, start and stop them, see what is running right now, and get remembered services re-created on reconnect |
| **Transfers** | Live and finished transfers with progress, speed and totals, and every background job labelled by kind (copy, move, rename, delete) |
| **Plugins** | Install, enable, disable and uninstall plugins, drive their declarative settings panel, restore a built-in plugin you removed, and open the plugin directory |
| **Settings** | RC endpoint and credentials, theme, accent colour, UI language, the embedded HTTP server port, the winget mirror source, the config file path, and `rcgui.conf` import/export |

## Bundled plugins

Plugins live in their own repositories; this repository only keeps the lock file `plugins/manifest.json` that pins a version, an asset name and a SHA-256 for each of them.

| Plugin | Version | What it does |
| --- | --- | --- |
| [imagehost](https://github.com/h897105167/rcgui-plugin-imagehost) | 0.6.1 | Uploads local images to an rclone remote, hands back a direct link (and an optional short link), and exposes an HTTP upload service that other apps can post to |
| [oneclick](https://github.com/h897105167/rcgui-plugin-oneclick) | 0.3.3 | Finds, verifies, downloads and starts a local rclone, then keeps it running for as long as RCGUI is open (Windows only) |

![The one-click rclone plugin panel](docs/oneclick-plugin-panel.png)

## Requirements

| Item | Notes |
| --- | --- |
| Windows 10 / 11 (x64) | The only supported target today |
| [Rust](https://rustup.rs/) with the MSVC toolchain | Needed to build from source |
| `wasm32-wasip1` target | `rustup target add wasm32-wasip1` — plugins are WASI modules |
| [WinFsp](https://winfsp.dev) | Optional; only needed for the **Mounts** page, and installable from inside the app |
| A running `rclone rcd` | Optional at build time; required at runtime unless a plugin starts one for you |

## Quick start

```powershell
git clone https://github.com/h897105167/rclone_gui_windows.git
cd rclone_gui_windows
.\scripts\build.ps1
```

`build.ps1` fetches the pinned plugin archives, verifies their SHA-256, builds the workspace and leaves the executable in `target\release\rclone-gui.exe`.

| Parameter | Effect |
| --- | --- |
| `-Release` | Build the release profile (the default is debug) |
| `-Run` | Launch the app as soon as the build succeeds |
| `-SkipPlugins` | Do not fetch or refresh plugin archives |
| `-ForcePlugins` | Re-download the plugin archives even when the cached copies match the lock file |

## Building by hand

The plugin archives have to be in place before `cargo build`, because they are embedded into the binary:

```powershell
.\scripts\fetch_plugins.ps1
cargo build -p rclone-gui
```

`fetch_plugins.ps1` reads `plugins/manifest.json`, downloads each release archive from the plugin repository, verifies its SHA-256 against the lock file and unpacks it into `crates/app/assets/plugins/`.

## Connecting to rclone

RCGUI talks to rclone's RC API, so `rclone` has to be listening somewhere. A typical session:

```powershell
rclone rcd --rc-addr 127.0.0.1:5574 --rc-user gui --rc-pass <your-password>
```

Then put the same address and credentials into **Settings** and press **Connect / handshake**. If rclone was started with `--rc-no-auth`, tick *no authentication* instead — otherwise every endpoint except `core/version` answers 403.

Mounts and services have no persistence of their own in rclone: `rclone mount` and `rclone serve` live only as long as the process that started them. RCGUI therefore remembers the ones you created and re-applies them through the RC API after every successful connect, skipping whatever is already mounted or already listening, and reporting per-item failures without blocking the rest. Unmounting or stopping an item forgets it. If the RC endpoint has no authentication configured at all, the replay is skipped — the connection status already says so.

## Embedded HTTP server

RCGUI can expose a small HTTP server (default port **5273**, configurable in Settings) so external tools and plugins can reach it:

| Route | Purpose |
| --- | --- |
| `GET /api/health` | Liveness probe for the app itself |
| `ANY /plugin/{id}` | Dispatch to the plugin whose id is `{id}` |
| `ANY /plugin/{id}/{*rest}` | Same, with an arbitrary sub-path |
| `ANY /api/plugin/{id}/{*rest}` | Legacy alias of the route above, kept for older clients |

A plugin opts into HTTP handling with `"http_entry": true` in its `config.json`; requests are then negotiated through the plugin's `http` capability, one JSON round per invocation.

## How plugins work

A plugin is a directory with a `config.json` (identity, version, capabilities, entry point) and a `main.wasm` compiled for `wasm32-wasip1`. There is no plugin API in the host UI: the panel is described **declaratively** by `plugin_setting.controls` in `config.json`, and the host renders whatever the plugin declares without knowing any specific plugin.

- **Capabilities.** A plugin must declare and then request each capability it uses: `kv`, `ui`, `fs`, `http`, `proc`, `net`, `rclone`, `env`. Dependencies are checked by the host, and every call is a JSON request/response round over stdin/stdout.
- **Localisation.** Display strings — labels, hints, option labels, `tags` — may be written as `{"zh": "…", "en": "…"}`. The host passes the active language to the plugin on every call (`params.lang`), and stores the value the user picked rather than the display string.
- **Lifecycle.** A plugin may declare `on_start` / `on_exit` hooks, may be disabled, and may be uninstalled — a built-in plugin leaves a tombstone behind so it can be restored from the Plugins page instead of disappearing for good.
- **Integrity.** The build-time archives are verified once when fetching and again when the app unpacks them into `%APPDATA%\RCGUI\plugins\<id>\`.

The full contract — manifest schema, capability signatures, action protocol — is in [`docs/PLUGIN.md`](docs/PLUGIN.md).

## Data directory

Configuration and runtime state are separated:

- `%APPDATA%\RCGUI\` — `config.json` (RC endpoint, server port, UI language, per-remote proxy bindings, the mounts and services you asked for), profiles, plugin copies and kv stores.
- On first start RCGUI copies an existing `%APPDATA%\rclone-gui\` directory over, so upgrading from the old name keeps your settings.
- **Portable mode:** drop an empty file named `portable` next to `rclone-gui.exe` and everything moves to `data\` beside the executable.

`rcgui.conf` is the versioned import/export format for a profile (remotes, mounts, services). Credentials are stripped by default, so an exported file is safe to carry around.

## Development

```powershell
cargo build --workspace
cargo test --workspace
cargo run -p plugin_host --example protocol_test
```

## Project layout

```
crates/
  app/       the Slint UI, the RC client wiring and the HTTP server
  i18n/      Chinese/English dictionaries and the `t` / `f` helpers
  profile/   the versioned `rcgui.conf` import/export format
  rc/        a typed client for rclone's RC API
  rclone/    legacy process helpers (exe discovery, a managed `rcd` child, `cat`
             streaming); the RC client has moved to rc/ and this crate is slated
             for removal
  plugin/    the WASI plugin host, capability broker and manifest handling
plugins/
  manifest.json   pinned plugin versions plus their SHA-256
scripts/     build.ps1, fetch_plugins.ps1, packaging helpers
docs/        PLUGIN.md, CHANGELOG.md, verification notes
```

## Documentation

| File | Contents |
| --- | --- |
| [`docs/CHANGELOG.md`](docs/CHANGELOG.md) | Everything that changed, newest first |
| [`docs/PLUGIN.md`](docs/PLUGIN.md) | Plugin manifest, capability groups, action protocol, HTTP entry points |
| [`docs/proxy-verification.md`](docs/proxy-verification.md) | What each rclone backend actually accepts for a proxy, measured |

## Security notes

- The RC password is stored in cleartext in `config.json`. Use a dedicated `--rc-user/--rc-pass` pair, and keep the RC port on loopback.
- Plugins run inside a WASI sandbox and reach the system only through declared capabilities. `fs.`, `proc.` and `http.` are the dangerous ones — read a plugin's `config.json` before you trust it.
- The embedded HTTP server binds to loopback and has no authentication of its own; treat anything you expose through a plugin's `http_entry` as reachable by any local process.
- `rclone obscure` obfuscates configuration values, it does not encrypt them: an obscured password in `rclone.conf` is recoverable.
