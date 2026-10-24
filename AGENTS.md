# RCGUI Refactoring Guide for Coding Agents

This document guides coding agents working on the refactor. The goal is to gradually turn this project into **RCGUI: a Windows GUI for managing rclone remote, mount, and serve configurations**. Read this guide and the repository documentation before inspecting or changing code. Do not treat it as a request to rewrite every module at once.

## 1. Product Goals and Boundaries

- Use **RCGUI** as the product display name. Internal crate, directory, and executable names may be migrated in stages; avoid breaking builds or user data just to rename them.
- The user installs and runs rclone. The application must not embed or extract rclone, or automatically start, stop, or restart an rclone process.
- By default, read state from and call APIs on a local rclone RC endpoint explicitly selected by the user. Do not assume that finding an `rclone.exe` process means an RC service is available.
- The application may perform user-requested mount/unmount, serve/start/stop, and remote configuration operations through the RC API. These operations are distinct from starting or terminating the rclone process.
- WinFsp is a separate optional dependency for mounting. Keep its detection, installation guidance, and mount error messages separate from rclone installation and connection status.
- RCGUI stores its own copies of remote configurations, desired mount/serve configurations, and profiles. Display runtime state returned by rclone separately. Do not treat current runtime state as a complete, reconstructable configuration.
- The primary feature scope is remotes, mounts, serves, connection status, and profile import/export. First inventory and keep the legacy file management, transfers, plugins, embedded HTTP, and other unrelated features buildable. Do not remove or extensively modify them without an explicit user request.

## 2. Protect the Working Tree

This repository may contain uncommitted user changes and untracked files. At the start of each phase, run and inspect:

```powershell
git status --short
git diff --stat
```

- Do not use `git reset`, `git checkout --`, `git clean`, or recursive deletion to clean the working tree.
- Do not overwrite user changes unrelated to the current phase. Read the current file and its diff before editing an existing file.
- Do not delete `vendor/`, LFS files, build scripts, or existing application data directories until all references have been checked and the impact has been explained.
- If requirements conflict with code behavior, continue the inventory and work that do not depend on the decision. Raise only the smallest necessary question in the phase report; do not expand the product scope on your own.

## 3. Connection and Runtime State

### Connection

- Rclone must be started with `--rc` to provide the RC API. The connection UI must let the user select or enter an endpoint. It may suggest `http://127.0.0.1:5572` as a default, but must allow a custom address, port, and authentication.
- Do not scan arbitrary local ports or send control requests to unknown local services. RC access is powerful; default to loopback and never write authentication information to ordinary logs.
- After connecting, query the rclone version, actual configuration path, and supported features through RC before reading remotes, mounts, and serves. Clearly report missing APIs or unsupported older versions.
- If no rclone instance is reachable, keep the GUI open in offline mode. The user must still be able to view, edit, import, and export local profiles. Provide retry and reconnect actions.
- Allow users to save multiple connections/profiles. The first release may support only one active endpoint, but must not merge data from multiple rclone instances into indistinguishable state.

### Keep Data Sources Separate

- **Configured remotes:** obtained from the active rclone configuration/API.
- **Live mounts and serves:** obtained from RC APIs such as `mount/listmounts` and `serve/list`.
- **RCGUI profiles:** persisted locally by RCGUI and describe the configuration the user wants to save or restore.
- In the UI, distinguish “running now,” “saved,” “pending application,” and “disconnected/stale.” When disconnected, preserve the last snapshot but mark it stale.
- `mount/listmounts` is not guaranteed to return every option needed to recreate a mount. Do not generate a complete mount profile from the runtime list alone. Persist the user's original mount and serve parameters.

## 4. Configuration Files, Profiles, Import, and Export

- Store RCGUI data in its own user-data directory. When migrating data from the old `%APPDATA%\\rclone-gui` directory, copy/migrate it first and retain a rollback path; do not delete the old directory outright.
- Define a versioned single-file import/export format using the user-requested filename `rcgui.conf`. Include at least a schema version, profile metadata, rclone configuration, desired mount/serve configurations, and an optional script manifest.
- Specify the package format and import rules before implementation. If using an archive container, protect against path traversal, duplicate/overwriting paths, oversized files, and integrity-check failures. Write only to RCGUI-managed locations.
- Parse and validate an import first. Then show the profile name, remote names, mount/serve counts, target rclone configuration path, and merge/overwrite differences before applying it. The original configuration must remain recoverable if import fails.
- Exports may contain OAuth tokens, passwords, and cloud-service credentials. Never log credentials by default. Clearly disclose that an export contains sensitive data and explicitly handle encrypted configurations. Rclone's `obscure` format is not encryption.
- Do not silently replace a configuration file while rclone is using it. Prefer supported RC config APIs for updating remotes. For a full-file replacement, make a backup and mark the change as requiring the user to restart/reconnect rclone manually. RCGUI does not restart the process.
- Obtain the actual configuration path from the connected instance; do not assume `%APPDATA%\\rclone\\rclone.conf`.
- Define “export current configuration” precisely. Remotes can be read from the active rclone configuration; mount/serve exports should prefer complete parameters saved by RCGUI. If runtime state is missing fields, disclose them in the export preview instead of silently creating an incomplete profile.

## 5. Startup Scripts and Automatic Restore

- Do not assume rclone automatically executes scripts registered in `rclone.conf`. Windows logon startup is handled through the user's Startup folder, Task Scheduler, or an external launcher.
- Version 1 may generate an explicit PowerShell restore script. It must connect to an **already running rclone instance with RC enabled**, wait for a successful RC health check, and then restore mounts/serves in the saved order. If the endpoint is unavailable, it must exit with an error; it must not start or terminate rclone.
- Scripts inside an imported package are executable code. Preview their contents and destination paths. Do not execute them, create scheduled tasks, or silently register logon startup unless the user explicitly chooses to do so.
- Do not discover and delete scripts by filename prefix. Use an RCGUI-specific directory and a manifest that records generated files and versions. Only clean up files confirmed by the manifest as RCGUI-managed.
- Generate PowerShell with argument arrays or safe serialization. Avoid constructing commands by string concatenation, which can misinterpret spaces, quotes, `&`, `%`, and similar characters.
- Keep “rclone process lifecycle” separate from “mount/serve runtime state.” RCGUI does not start/exit rclone, but may create/stop mounts or serves through the RC API after an explicit user action.

## 6. Refreshing and UI Behavior

- Refresh immediately after connecting, when the window returns to the foreground, and when the user requests a refresh.
- Run lightweight state polling only while the window is visible and in the foreground. Pause when minimized or hidden to the system tray. Refresh immediately on restore and update the “last refreshed” time.
- Do not allow overlapping polls of the same kind. On timeout, cancel or ignore stale results and use backoff before retrying. An older response must never overwrite newer UI state.
- Poll only lightweight remote/mount/serve/connection state. Do not periodically refresh remote file listings, probe remote backends, or repeat mount/serve operations.
- Report connection errors, RC API errors, missing WinFsp, occupied drive letters, and slow remote backends separately. Avoid displaying a generic “mount failed” message for all cases.

## 7. Recommended Implementation Phases

1. **Inventory:** Inspect the workspace, `crates/app`, `crates/rclone`, `build.rs`, build scripts, LFS assets, configuration migration, and every process-management entry point. Produce a dependency map and split plan; do not delete files yet.
2. **Decouple the RC client:** Separate connection/authentication/API calls to an external endpoint from launching/stopping rclone child processes. Migrate callers before removing process-management code.
3. **Connection state and offline UI:** Implement connect/disconnect/retry, endpoint configuration, connection handshake, live-data refresh, and offline profile management.
4. **Data model and persistence:** Establish versioned remote/mount/serve profile models, separate persistent configuration from runtime state, and account for legacy application configuration.
5. **Import/export:** Implement the `rcgui.conf` format, preview, merge/replace, backups, sensitive-data handling, and failure recovery.
6. **Restore script:** Generate and preview scripts from structured profiles. Scripts may connect only to an already running RC instance; they must not start or terminate rclone.
7. **Cleanup and documentation:** Remove embedded rclone assets, extraction/download scripts, and old process management only after confirming no references remain. Update the README, build instructions, application name, and data-directory migration notes.

At the end of each phase, report what changed, affected files, remaining assumptions, risks, and the next phase. Do not compress the entire refactor into one large unchecked rewrite.

## 8. Build and Command Constraints

- Read the current `README.md`, relevant crate `Cargo.toml` files, `build.rs`, and PowerShell build scripts before choosing build commands. The README currently describes an embedded-rclone workflow that may remain necessary until migration is complete.
- Plugins are **not** in this repository and are **not** compiled into the executable. Before building, `scripts/fetch_plugins.ps1` downloads each plugin's release zip listed in `plugins/manifest.json` and verifies its sha256; `crates/app/build.rs` only copies `crates/app/assets/plugins/` next to the binary. There is no `include_bytes!`/`IMAGEHOST_WASM` step any more, so the old "build the WASM plugins first" ordering rule is obsolete — but never drop the fetch step or ship without plugin assets. Plugin framework and integration rules: `docs/PLUGIN.md`.
- Do not run the test suite unless the user explicitly asks for testing or verification. Explain the purpose before running other build/formatting commands. Do not install tools, modify system services, register startup tasks, or access the network unless necessary.
- Never expose RC passwords, OAuth tokens, or full sensitive configuration in code, command output, screenshots, or documentation.

## 9. Completion Criteria

- With no rclone process running, RCGUI can start and edit/import/export local profiles without launching rclone.
- RCGUI displays live state only after successfully handshaking with an explicitly selected RC endpoint.
- Remote, mount/serve profiles, and live runtime state remain distinct; disconnecting does not clear local data.
- Import does not silently overwrite a configuration in use; export and script execution provide clear previews and sensitive-data notices.
- Mount/serve control calls the RC API only after an explicit user action. RCGUI does not call `core/quit`, kill rclone processes, or silently create logon startup tasks.
- Build documentation matches actual dependencies. Before removing embedded rclone assets, verify that all compile-time and runtime references have been migrated.
