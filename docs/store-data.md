# Store and direct-download data

The `microsoft-store` Cargo feature selects the Store edition. Before Tauri
starts, `src-tauri/src/distribution.rs` changes its application identifier from
`com.christoffer.canvaz` to `com.christoffer.canvaz.store`. This identifier is
for Tauri's application paths; it is separate from the MSIX identity assigned
by Partner Center. Keep both editions' identifiers stable across upgrades.

This makes Tauri's data, local-data, configuration, cache, and log paths follow
the edition, including plugin path resolution. Tauri's default Windows/Linux
webview profile also follows the identifier. No individual preference, session,
or browser-storage path needs to guess which edition is running.

| Windows location | Direct download | Store |
| --- | --- | --- |
| Preferences and session restore | `%APPDATA%\com.christoffer.canvaz` | `%APPDATA%\com.christoffer.canvaz.store` |
| WebView2 profile/cache root | `%LOCALAPPDATA%\com.christoffer.canvaz` | `%LOCALAPPDATA%\com.christoffer.canvaz.store` |

Windows package virtualization can redirect the Store paths within per-user
package storage. On Linux, data and webview profiles normally use
`$XDG_DATA_HOME/<identifier>`, defaulting to `~/.local/share/<identifier>`.
Demo builds continue to use temporary directories for preferences and sessions.

The Store edition starts with no imported preferences, project history, or
session. Both editions can be installed without writing the same automatic
session or webview profile. User-selected project files, DBC files, recordings,
and exports remain wherever the user chooses; they are not copied or relocated.
Avoid editing the same project or recording file from both editions at once.
CAN devices may also restrict simultaneous access by multiple processes.

## Building and checking

For a complete Windows Store build with its bundled WebView2 runtime:

```powershell
py -3 scripts/build-msix.py
```

The dedicated builder isolates Cargo output and creates an unsigned MSIX.
See [Store packaging](store-packaging.md) for prerequisites, artifacts, and CI.
A bare Store-feature executable on Windows needs the pinned runtime directory
named `WebView2` beside it; use the package layout for native testing.

Direct-download release builds must omit `microsoft-store`. The feature also
disables the GitHub updater and selects the bundled WebView2 runtime. Native
installation and certification checks remain required before submission.

## Updates

The compiled backend reports the update channel before the frontend can make
any GitHub request. Store builds replace **About → Check for Updates** with
**Microsoft Store updates…** and explain the channel in the About dialog.
The update dialog offers **Open Microsoft Store**, which opens the fixed
`ms-windows-store://downloadsandupdates` URI through a backend command.
This does not require a product ID and never opens Pigtail's product listing.
If Store launch fails, the dialog asks the user to open Microsoft Store manually;
it does not fall back to a GitHub installer.

Startup checks, manual checks, and stale GitHub dialog actions are blocked for
Store builds. Backend support/download/install commands independently reject
Store builds before accessing installer state, making HTTP requests, or starting
an installer. Copied preferences, including a skipped GitHub version, cannot
enable this updater. Demo builds also have updates disabled. If the frontend
cannot determine the backend's channel, it disables updates without contacting
GitHub. Direct-download builds keep their existing GitHub update behavior.

Canvaz does not request Store update installation, termination, or restart.
Windows, Microsoft Store settings, and enterprise policies determine external
update timing; an active CAN session does not guarantee that an update cannot
interrupt the app. Store and GitHub releases can become available at different
times.

Before publication, verify the Store menu opens Microsoft Store on Windows and
that no GitHub request or update notice occurs on startup or manual update
actions, including with copied preferences. See [Microsoft's Store URI guidance](https://learn.microsoft.com/en-us/windows/apps/develop/launch/launch-store-app).

Regression checks exercise Tauri's actual path resolver for both editions and
restore a Store session alongside untouched direct-edition fixture files:

```sh
cargo test --manifest-path src-tauri/Cargo.toml --locked --lib distribution::tests
cargo test --manifest-path src-tauri/Cargo.toml --locked --features microsoft-store --lib distribution::tests
```

Before publication, test both Windows editions installed together, first-run
preferences, session restore after restart, distinct WebView2 profiles, and an
MSIX upgrade with saved data present. Linux/mock-runtime tests do not verify
Windows package virtualization or Store update delivery.

## Optional manual migration

1. Launch and close the Store edition once to create its data directory.
2. Close both editions and back up the destination files.
3. Copy `preferences.json` and, if wanted, `last-session.canvaz` and
   `last-project.txt` from the direct edition's data directory to the Store
   edition's data directory. Use the effective packaged location on Windows.
4. Keep any referenced DBC/project files accessible at their saved paths, or
   select their new locations when reopening them.

Do not copy live webview profiles. Recordings and exports need no migration
unless you choose to move them. Back up important files before uninstalling;
do not rely on package removal to preserve settings or recordings.

References: [Tauri application data paths](https://docs.rs/tauri/2.11.6/tauri/path/struct.PathResolver.html#method.app_data_dir)
and [webview profile selection in the locked Tauri version](https://docs.rs/crate/tauri/2.11.6/source/src/manager/webview.rs).
