# RCGUI Guide for Coding Agents

RCGUI is a Windows GUI for managing rclone remotes, mounts and serves, written in Rust with a Slint (native, no webview) interface. This document is the working agreement for coding agents: what must not break, which documents are authoritative, and how to build and verify. Read it, plus [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), before changing code.

Version in the tree is **0.3.2 in development**. The newest released version is **v0.2.1**. Do not describe in-development work as released, in code, docs or commit messages. Neither `0.3.0` nor `0.3.1` got a tag; the batch the docs call "0.3.0 development" ships as **0.3.2**. Plugin packages declare the floor they actually need: the new plugin protocol starts at `min_gui_version = "0.3.1"`, and a plugin whose panel state has to follow what the user is still typing needs `"0.3.2"`, because only 0.3.2 and later re-ask the plugin for state after each keystroke.

## 1. Product Boundaries

- **RCGUI** is the product name users see. Crate, directory and executable names may migrate in stages; never break a build or a user's data just to rename something.
- **Two ways to reach rclone, one default.** `managed core` means RCGUI starts a child `rclone.exe` and owns its lifetime. `external endpoint` means the user (or a script) started rclone and RCGUI only connects. Managed core is the default; external stays fully supported because it is the only option for a shared or already-tuned setup.
- **Never touch a process RCGUI did not start.** For an attached external endpoint, no code path may call `core/quit`, kill the process, or restart it. `mount/*` and `serve/*` calls are runtime operations and remain allowed; process lifetime is not the same thing as mount state, and code must keep them distinguishable.
- Do not scan local ports or send control requests to unknown local services. Managed core binds loopback only. Do not assume that finding an `rclone.exe` process means an RC service is available.
- After connecting, read the rclone version, the real configuration path and the supported feature set through RC before touching remotes, mounts or serves. Report missing APIs plainly instead of degrading silently.
- If no endpoint is reachable, the GUI stays usable: local profiles can still be viewed, edited, imported and exported. Provide retry and reconnect.
- WinFsp is a separate optional dependency for mounting. Its detection, installation guidance and error messages stay separate from rclone installation and connection status.
- RCGUI keeps its own copy of desired state (remotes, mount/serve parameters, profiles) and shows rclone's runtime state separately. Runtime state is not a complete configuration and must never be used to reconstruct one.
- Active feature scope: remotes, mounts, serves, connection status, profiles. Keep the legacy file management, transfers, plugins and embedded HTTP buildable; do not remove or rework them without an explicit request.

## 2. Authoritative Documents

| Topic | Document |
|---|---|
| Layers, dependency rules, concurrency/cache/credential/accessibility contracts | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) |
| Plugin protocol, manifest schema, capability list | [docs/PLUGIN.md](docs/PLUGIN.md) |
| Version numbering and changelog rules | [docs/VERSIONING.md](docs/VERSIONING.md) |
| Release assets, naming, checksums, reproducibility | [docs/RELEASE.md](docs/RELEASE.md) |
| Command-line contract (including what is not implemented yet) | [docs/CLI.md](docs/CLI.md) |
| Performance budget and how to measure it | [docs/PERFORMANCE.md](docs/PERFORMANCE.md) |
| Contribution flow, review gates, how to add a plugin to the store | [CONTRIBUTING.md](CONTRIBUTING.md), [docs/store/README.md](docs/store/README.md) |

If a document and the code disagree, the code is the current truth and the document is a bug — report it rather than quietly following either one.

## 3. Protect the Working Tree

Start each session with:

```powershell
git status --short
git diff --stat
```

- Never use `git reset`, `git checkout --`, `git clean` or recursive deletion to tidy the tree.
- Read a file and its diff before editing it. Do not overwrite changes that belong to someone else's task.
- Do not delete `vendor/`, LFS assets, build scripts or existing application data directories until every reference has been checked and the impact explained.
- If requirements collide with code behaviour, keep doing the work that does not depend on the decision, and raise the smallest necessary question in the report. Do not widen the scope on your own.

## 4. Connection, Core and Runtime State

- Managed core: RCGUI allocates a free loopback port, generates the RC credentials, starts the child with the correct `--rc` flags, waits for a successful handshake, and records the real port in state. A port collision is its own error, never folded into "start failed".
- The child is bound to a Job Object so it cannot outlive RCGUI. Do not leave orphan `rclone.exe` behind on crash or forced exit.
- Automatic restart is off by default. A crashed core shows "stopped + reason + retry"; silent restarting hides configuration errors and loops.
- External endpoint: the user supplies address, port and credentials. Default suggestions use loopback. Disconnecting stops polling and keeps the last snapshot marked stale; it never clears local data.
- One active endpoint at a time in 0.3.0. Never merge data from two rclone instances into indistinguishable state.
- Distinguish in the UI: "running now", "saved", "pending application", "disconnected/stale".

## 5. Configuration, Profiles, Import and Export

- RCGUI data lives in its own user-data directory. When migrating from `%APPDATA%\rclone-gui`, copy first and keep a rollback path; never delete the old directory.
- The import/export container is the versioned single file `rcgui.conf`: schema version, profile metadata, rclone configuration, desired mount/serve configurations, optional script manifest.
- Validate before applying, then preview: profile name, remote names, mount/serve counts, target rclone configuration path, merge/overwrite differences. A failed import must leave the original configuration recoverable.
- Exports can contain OAuth tokens, passwords and cloud credentials. Never log them. State clearly that an export is sensitive, handle encrypted configurations explicitly, and remember that rclone's `obscure` is obfuscation, not encryption.
- Do not silently replace a configuration file that a running rclone is using. Prefer RC config APIs. For a full-file replacement, take a backup and mark that the user must reconnect — and in managed mode, that RCGUI will restart the core it owns.
- Read the actual configuration path from the connected instance; never assume `%APPDATA%\rclone\rclone.conf`.

## 6. Startup Scripts and Restore

- rclone does not execute scripts registered in `rclone.conf`. Windows logon startup is the user's Startup folder, Task Scheduler, or an external launcher.
- A generated PowerShell restore script connects to an **already running** RC endpoint, waits for a health check, then replays mounts/serves in saved order. It must not start or stop rclone: it cannot own a process. In managed mode the GUI can do the start itself; the generated script still may not.
- Scripts inside an imported package are code. Preview contents and destinations; never execute, never register scheduled tasks or logon startup without an explicit user choice.
- Never discover or delete scripts by filename prefix. Use an RCGUI-owned directory plus a manifest that records what was generated and at which version.
- Build PowerShell from argument arrays or safe serialization, not string concatenation.
- Keep "rclone process lifecycle" separate from "mount/serve runtime state" in code, tests and messages.

## 7. Refresh and UI Behaviour

- Refresh on connect, on window restore, and on explicit user request.
- Poll lightweight state only while the window is visible and focused. Pause when minimised or hidden to the tray; refresh immediately on restore and update "last refreshed".
- Never overlap polls of the same kind. Cancel or discard stale results; an older response must not overwrite newer state.
- Poll remote/mount/serve/connection state only. Do not periodically re-read remote listings or probe backends.
- Report connection errors, RC API errors, missing WinFsp, occupied drive letters and slow backends separately. "Mount failed" is not an acceptable catch-all.
- Accessibility is a requirement, not a nice-to-have: full keyboard reach, readable names on every control including icon buttons, visible focus, contrast ≥ 4.5:1, status never conveyed by colour alone, and respect for the OS "reduce motion" setting. Details and the honest limits of the static accessibility export are in [docs/ARCHITECTURE.md §4.5](docs/ARCHITECTURE.md).

## 8. Build and Command Constraints

- Read `README.md`, the relevant crate `Cargo.toml`, `crates/app/build.rs` and the scripts before choosing commands. The workspace version is the single source of truth for the product version; member crates inherit it via `version.workspace = true`.
- Plugins are not in this repository and are not compiled into the executable. `scripts/fetch_plugins.ps1` downloads each release zip pinned in `plugins/manifest.json` and verifies its sha256 (twice: zip, then the package's own `check.sha256`); `crates/app/build.rs` only copies `crates/app/assets/plugins/` next to the binary. Never drop the fetch step, never ship without assets, and never weaken hash checking to "warn on mismatch".
- Release packaging uses `scripts/build-release.ps1` (portable + checksums), `scripts/fetch_core.ps1` (rclone core pinned in `packaging/core-manifest.json`, sha256 verified), and `packaging/RCGUI.iss` (Inno Setup). The manifest holds two sources; `default_source` is `extra` (gulp79/rclone-extra, extra backends such as alist, alldebrid, teldrive, terabox) and `upstream` is plain rclone. `-CoreSource` selects one and never silently falls back; a non-default source suffixes the artifact names with `-<source>`. `-SkipPlugins` / `-SkipCore` skip downloads, never verification. A local unverified `rclone.exe` may be used for a private test build but never for a published artifact.
- Do not run the test suite unless the user asks for testing or verification. Say what a command is for before running build or formatting commands. Do not install tools, change system services, register startup tasks or use the network unless it is necessary for the task.
- Never expose RC passwords, OAuth tokens or full sensitive configuration in code, output, screenshots or documentation.

## 9. Honesty Rules for Docs, Store and Releases

- Never claim a feature is shipped when it is only designed. `docs/ARCHITECTURE.md` marks every layer `已有` / `部分` / `规划`; keep that discipline in README, changelog and store listings.
- The plugin store lists only plugins that exist as a real release, with the sha256 of that release asset. No placeholder URLs or invented hashes. The K-Vault plugin owns the whole site protocol — marking a remote, listing, uploading (URL, auth header and form fields described by the plugin), deleting, and migrating old settings — while the app keeps only the generic discovery, routing and "no plugin, so this remote is not operable" rules, and calls whatever the plugin declares. Since that split needs the new plugin protocol, such packages must declare `min_gui_version = "0.3.1"`; a plugin whose panel has to follow what the user is still typing declares `"0.3.2"`, because only 0.3.2 and later re-ask the plugin for state after each keystroke. Add K-Vault to the store only after its first verified release exists, with a `min_gui_version` equal to the app version that really carries the behaviour it needs — `scripts/validate-store-index.ps1` compares it against the value inside the package. That is now the case: the store index lists `kvault` 0.3.1 with `min_gui_version` 0.3.2, and the package is deliberately not in `plugins/manifest.json`, so it is installed from the store rather than shipped with the portable build.
- `oneclick` is deliberately absent from the store and from the portable package.
- Badges, screenshots and numbers must be real. A hand-written SVG is fine if it is labelled as a diagram, not as a screenshot. Benchmarks come from `scripts/measure-perf.ps1` on named hardware; never invent or extrapolate a figure.
- The static accessibility tree export describes what RCGUI intends to expose. It is not the live Windows UIA tree and must not be presented as accessibility verification.
- No secrets, development data, absolute local paths or unverified binaries inside release artifacts. Verify before packaging; a checker script passing only proves the checker ran.

## 10. Completion Criteria

- With no rclone running, RCGUI starts, stays usable, and can view/edit/import/export local profiles; managed core can be started from the UI when the user asks for it.
- Live state appears only after a successful handshake with the selected or self-started endpoint, and the UI shows which mode it is in.
- Remotes, saved profiles and runtime state stay distinct; disconnecting never clears local data, and an attached external process is never killed or quit.
- Import never silently overwrites a configuration in use; export and script execution always preview and warn.
- Mount/serve control happens only after an explicit user action; no `core/quit`, no killing foreign processes, no silent logon startup.
- Documentation matches the code, including everything still marked as planned. `docs/CHANGELOG.md` reflects what shipped, not what was attempted.
