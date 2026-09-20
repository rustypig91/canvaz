# Rusty's Canvaz - CAN Analyzer

Rusty's Canvaz is the CAN analyzer in the Rusty's toolchain family, alongside Rusty's Pigtail - Serial Terminal. View, decode, plot, and simulate CAN bus traffic using DBC files.

## Features

- **Trace** — live frame table with filtering by channel, CAN ID, message name, DLC, direction, cycle time, and data bytes
- **Plot** — real-time signal plots with zoom, multi-signal panes, and drag-and-drop from the DBC browser
- **Simulate** — send DBC-defined messages or raw frames on a configurable interval
- **DBC browser** — load a DBC file per channel, browse messages and signals, see live decoded values (including value-table enums) and min/max
- **J1939** — per-channel protocol mode adding PGN / priority / SA / DA columns to the trace and passive transport-protocol reassembly (BAM and RTS/CTS)
- **Project files** — save and restore your channel, plot, and simulator configuration (`.canvaz`)
- **Session restore** — last working state is restored automatically on startup
- **Offline channels** — missing interfaces appear red (disconnected), while DBC browsing, plots, and simulator configuration remain editable. Click *Start* to rescan and run available channels; channels that cannot start report an error without blocking the others. Use *Reload backends* to refresh their status.
- **System resources** — built-in dialog showing the app's CPU and memory usage

## Hardware support

| Backend | Platforms | Runtime requirement |
|---------|-----------|---------------------|
| Kvaser CANlib (`canlib32.dll` / `libcanlib.so`) | Windows, Linux | [Kvaser drivers](https://www.kvaser.com/download/) |
| PEAK PCAN-Basic (`PCANBasic.dll` / `libpcanbasic.so`) | Windows, Linux | [PCAN-Basic API](https://www.peak-system.com/products/software/development-packages/pcan-basic/) |
| SocketCAN, including virtual `vcan` | Linux | — |

All backends are included in a default build (SocketCAN automatically on
Linux). Kvaser and PCAN load their driver library at runtime; a missing driver
just disables that backend rather than preventing startup.

## Building

**Prerequisites:** [Rust](https://rustup.rs/), [Node.js](https://nodejs.org/), [Tauri prerequisites](https://tauri.app/start/prerequisites/)

```sh
npm install
npm run tauri dev      # development
npm run tauri build    # production
```

### Linux startup from a Snap IDE

Canvaz removes inherited Snap GTK/GIO module paths before starting its native
WebKit runtime. This prevents the `libpthread.so.0: undefined symbol:
__libc_pthread_init, version GLIBC_PRIVATE` crash when launching from VS Code
installed through Snap. Host and AppImage library paths are preserved.

If you are running an older build, launch it from a terminal outside the Snap
IDE, or temporarily clear the affected variables for that command:

```sh
env -u GTK_PATH -u GTK_EXE_PREFIX -u GTK_IM_MODULE_FILE -u GIO_MODULE_DIR \
  -u GIO_EXTRA_MODULES -u GDK_PIXBUF_MODULE_FILE -u GDK_PIXBUF_MODULEDIR \
  -u GSETTINGS_SCHEMA_DIR npm run tauri dev
```

### Build Windows installers locally

Install Node.js, the Windows x64 MSVC Rust toolchain, and Visual Studio Build
Tools with **Desktop development with C++** and a Windows SDK. MSI packaging
also requires the Windows **VBSCRIPT** optional feature (see
[Tauri's Windows installer guide](https://v2.tauri.app/distribute/windows-installer/#building)). Tauri downloads its
NSIS and WiX packaging tools on the first build, so internet access is required.

From the repository root in PowerShell or Command Prompt:

```powershell
.\scripts\build-release.cmd
```

This installs the locked npm dependencies, builds the current frontend and Rust
code in release mode, and produces both unsigned Windows x64 installers:

- Setup EXE: `src-tauri/target/release/bundle/nsis/`
- MSI: `src-tauri/target/release/bundle/msi/`

For subsequent builds, reuse installed dependencies or select one installer:

```powershell
.\scripts\build-release.cmd -SkipDependencyInstall
.\scripts\build-release.cmd -Bundle nsis -SkipDependencyInstall
.\scripts\build-release.cmd -Bundle msi -SkipDependencyInstall
```

The `.cmd` launcher runs `build-release.ps1` with a process-only execution-policy
override, so unsigned-script restrictions do not require changing your PowerShell
settings. It prints the installer paths without starting an installation.

To test the installation UI, launch the setup EXE normally and then using
**Run as administrator**; **Only me (recommended)** should be the default in
both cases. The MSI is a separate managed-deployment installer.
Local builds retain the Git-derived app version; use **About > Check for Updates**
to check manually. End-to-end automatic updating requires a newer published
GitHub release with installer assets and SHA-256 digests.

### Build Linux packages locally

Use x86_64 Linux with Node.js and the native GNU Rust toolchain. On Ubuntu 22.04
or newer / Debian 12 or newer, install the build dependencies:

```sh
sudo apt update
sudo apt install build-essential pkg-config curl wget file \
  libwebkit2gtk-4.1-dev libssl-dev libxdo-dev libudev-dev \
  libayatana-appindicator3-dev librsvg2-dev patchelf
```

For other distributions, follow the [Tauri Linux prerequisites](https://v2.tauri.app/start/prerequisites/#linux)
and add the libudev development package and `patchelf`. Build on the oldest Linux
distribution you intend to support, since packages depend on the build system's
library versions. Internet access is needed for dependencies and packaging tools.

From the repository root:

```sh
bash scripts/build-release.sh
# Reuse installed npm dependencies:
bash scripts/build-release.sh --skip-dependency-install
# Build just one format (appimage, deb, or rpm):
bash scripts/build-release.sh --bundle appimage --skip-dependency-install
```

The script builds current frontend and Rust sources in release mode, then prints
the new packages under `src-tauri/target/release/bundle/appimage/`, `deb/`, and
`rpm/`. It does not install or launch them. The first build should run without
`--skip-dependency-install`, especially when switching from a Windows checkout.

To test an AppImage, make it executable with `chmod +x path/to/Canvaz.AppImage`
and run it. If FUSE is unavailable, run it with `--appimage-extract-and-run`.
For DEB or RPM, install the generated package using your distribution's package
manager. In-app updates apply to AppImage installations; DEB/RPM updates use the
package manager.

## Development

- Frontend: vanilla TypeScript + HTML/CSS, bundled with Vite
- Backend: Rust (Tauri v2)
- CAN decoding: custom DBC parser + signal codec, J1939 ID decoding and TP reassembly in `src-tauri/src/j1939.rs`

## Updates

For Windows, use the `-setup.exe` installer and choose **Only me (recommended)**,
even if you started setup as administrator. **All users** remains available for
shared installations. Automatic updates preserve the existing installation scope.
The MSI is available for managed deployments.

Release builds check GitHub for updates at startup. **About > Check for Updates**
checks manually, including versions you previously skipped. **Update and restart**
downloads the release, verifies its published SHA-256 checksum, saves the current
session, and installs it. Progress and retry are shown in the app.

Windows updates use the matching NSIS or MSI installer and reopen Canvaz when
installation finishes; Windows may request elevation. Linux AppImage updates
replace the AppImage in its existing folder (which must be writable) and restart.
Update `.deb` and `.rpm` installations through your package manager instead.
The downloads page remains available as a manual fallback.

The existing GitHub release workflow supplies the installers and AppImage.
Automatic installation requires GitHub's SHA-256 asset digest and a completed
upload; releases without a digest can still be downloaded manually.
