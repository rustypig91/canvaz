# Microsoft Store packaging and identity

## Selected route

Use an **x64 MSIX** containing the production Tauri/Win32 application. Register
it in Partner Center as an **MSIX or PWA app**. The existing NSIS/MSI installers
remain the direct-download route. `scripts/build-msix.py` builds the production
frontend and Store-feature executable, stages the package using shared Evergreen
WebView2, indexes scaled icons with MakePri, and runs MakeAppx with validation enabled.

The manifest template is `packaging/windows/msix/AppxManifest.xml.in`. It uses
`packagedClassicApp`, medium integrity, and `runFullTrust` for native CAN driver
APIs and desktop file access/dialogs. This does not convert Canvaz into a UWP
AppContainer or request administrator elevation. No driver, service, or separate
updater is declared in the package.

The initial declared target is x64 Windows 10 build 19041 or newer, with
`MaxVersionTested` set to build 26100, matching the Pigtail manifest baseline.
These are compatibility declarations, not a claim of completed native testing.
ARM64 and x86 packages are not planned for the initial submission.

Microsoft signs MSIX submissions during Store publication. A purchased signing
certificate is not required for this route. Local sideloading has separate
signing/development-registration requirements. EXE/MSI Store submissions would
require our own Authenticode signing, so they are not the chosen route.

## Registration status: Partner Center identity supplied

The user supplied Canvaz's Partner Center identity values, now recorded in
`packaging/windows/msix/store-identity.json`:

| Manifest field | Supplied value |
| --- | --- |
| Identity Name | `rustypig91.RustysCanvaz-CANAnalyzer` |
| Identity Publisher | `CN=B41BE868-9458-4B44-AE02-EA569AC9D53B` |
| PublisherDisplayName | `rustypig91` |

The configured display name remains **Rusty's Canvaz - CAN Analyzer**, matching
the current application title. Its exact reserved spelling/punctuation was not
included with the identity values; confirm the match before submission.
The supplied Store ID is **9P3PGB2ZZ67L**:

- Web listing: <https://apps.microsoft.com/detail/9P3PGB2ZZ67L>
- Store protocol: `ms-windows-store://pdp/?productid=9P3PGB2ZZ67L`

These are the product links supplied from Partner Center; the listing's public
availability has not been verified. The Store ID is not an MSIX manifest field.

The JSON file is the single source for the Store package identity. To refresh
its metadata, open the existing Canvaz product in
[Partner Center → Apps & games](https://partner.microsoft.com/dashboard/apps-and-games),
then **Product management → Product identity**. Do not create another product
or reuse Pigtail's package name or Store ID. Copy exact values as follows:

| JSON field | Partner Center value |
| --- | --- |
| `name` | `Package/Identity/Name` |
| `publisher` | `Package/Identity/Publisher` |
| `publisherDisplayName` | `Package/Properties/PublisherDisplayName` |
| `displayName` | Exact reserved product name, including punctuation |
| `storeId` | Store ID from the listing link on Product identity |

These are public package metadata, not passwords or certificate secrets.
The Store ID identifies the product for listing links and is not needed by the
existing **Open Microsoft Store** updates link.
The PFN and Package SID are not substitutes for the package name or Store ID.

The Tauri storage identifier `com.christoffer.canvaz.store` is independent of
all these values. Do not replace it with a Partner Center package name or PFN:
doing so would change [the Store edition's existing data locations](store-data.md).

## Package version

The application version in `src-tauri/Cargo.toml` is the single source for all
release versions. The Store manifest and package filename use that version with
an appended `.0`: **1.0.0 → 1.0.0.0**, **1.1.0 → 1.1.0.0**, and
**1.1.2 → 1.1.2.0**. The current application version is **1.0.0**.
There is no separate version in the identity JSON or command-line override.

Store packaging requires a numeric `major.minor.patch` application version with
a nonzero major and each component at most 65535. Prerelease and build suffixes
are rejected. The fourth component is reserved for Store use and remains zero.
Increase the application version for every Store update, including packaging-only
revisions, so installed copies can upgrade. Use the matching `v<version>` release
tag and keep a record of the version submitted to Partner Center.

## Validate and prepare the manifest

With the supplied identity configured, run from the repository root:

```sh
python3 scripts/prepare-store-manifest.py --check
python3 scripts/prepare-store-manifest.py --output src-tauri/target/store/layout/AppxManifest.xml
```

The script requires Python 3.11+, uses only its standard library, and works from any working
directory. It checks required identity values, rejects template placeholders,
enforces the Store version format, and escapes XML values. It cannot verify
name availability, account ownership, or whether the values match Partner Center.
It does not create a product, build an executable, sign, package, upload, or
publish anything. The template is not a submission-ready manifest on its own.

## Dedicated Windows build

Install Python 3.11+, Node.js, Rust's Windows x64 MSVC toolchain, Visual Studio
C++ Build Tools, and the Windows 10/11 SDK (MakePri and MakeAppx).
From the repository root in PowerShell:

```powershell
py -3 scripts/build-msix.py
# Reuse node_modules, but always rebuild the frontend and Rust executable:
py -3 scripts/build-msix.py --skip-dependency-install
# Alternate output directory:
py -3 scripts/build-msix.py --output-directory release-assets/store
```

The script uses `src-tauri/target/store` independently of direct-download/demo
outputs and leaves the caller's environment and working directory alone.
Relative output directories resolve from the repository root. It runs `npm ci`
unless skipped, builds the frontend, and builds Cargo with explicit
`microsoft-store,kvaser,pcan,tauri/custom-protocol` features and no defaults.
`tauri/custom-protocol` embeds the production frontend instead of loading the
development server. A resolved-feature check refuses demo builds or missing
Store/CAN/custom-protocol features before packaging.

The child build uses a static MSVC CRT, preserving encoded Rust flag precedence
and whitespace-separated plain flags, to avoid a separate VC++ redistributable
dependency for Canvaz's executable. Verify native DLL dependencies on the first
Windows build. Inherited `TAURI_CONFIG` overlays are
removed from the build environment. All resources come from explicit paths;
the staging directory is unique and removed after success or failure.

The default output is:

```text
src-tauri/target/release-assets/store/canvaz-store-1.0.0.0-x64.msix
```

The adjacent `.sha256` and `.build-info.json` files record the checksum,
application/package versions, active features, Store identity, and the
Evergreen runtime mode. The MSIX is unsigned for Partner Center ingestion;
it is not a public unsigned sideload installer. This script does not install,
upload, or publish the package.

## Evergreen WebView2 on Windows

All Windows editions use the shared Evergreen WebView2 Runtime. The MSIX does
not contain a private browser runtime or runtime installer. Build metadata
records `"webview2": {"mode": "evergreen"}`; no runtime CAB is downloaded or cached.
Evergreen receives Microsoft's runtime updates independently of Canvaz's Store
updates, subject to the machine's update policies.

The EXE and MSI installers explicitly use Tauri's `downloadBootstrapper` mode:
they reuse the installed runtime and download/install Evergreen if it is missing.
The bootstrapper needs internet access when the runtime is missing. For offline
deployment, install Microsoft's Evergreen Standalone Installer beforehand.

The custom Store MSIX builder does not run Tauri's installer hooks, and this
package does not declare a Store-installed runtime dependency. Before creating
any webview, all Windows builds check for WebView2. If detection fails, a native
dialog offers to open Microsoft's download page. Users install the Evergreen
Runtime and relaunch Canvaz; declining exits cleanly, and a browser-launch failure
shows the download URL. The check never downloads or runs an installer itself.
This also covers a runtime removed after an EXE/MSI installation.

Windows 11 includes Evergreen, and most Windows 10 machines already have it.
Keep the missing-runtime recovery path for managed/custom installations. Test
the app regularly against current and upcoming runtimes, since their updates
are independent of Canvaz releases. See
[Microsoft's distribution guidance](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution).

## GitHub Actions

`.github/workflows/store.yml` runs on relevant PRs, all `main` pushes, and manual
dispatch. The release workflow calls it for `v*` tags. Only superseded PR runs
are cancelled. It tests the frontend, packaging orchestration, identity, and
Store-feature Rust code, then
uploads `canvaz-store-msix-<run-id>` with the MSIX, checksum, and build metadata.
Every successful Store run also creates `store-submission.zip` and uploads it
as `canvaz-store-submission-<run-id>`, including PR, `main`, and manual runs.
Artifact replacement supports reruns. The ZIP contains the package files,
all checked-in listing artwork and package icons,
identity/manifest references, license, privacy policy, and submission instructions.
It verifies that the MSIX matches its checksum and build metadata before creating
the archive. Missing package files or artwork directories fail the Store job and
block releases. The release workflow downloads this finished archive and attaches
it without repackaging. The Linux demo screenshot remains a separate release
asset; prepare Windows Store edition screenshots for submission. The archive
is a maintainer bundle; upload the contained MSIX to Partner Center. CI does not
submit to Partner Center automatically.

Tag builds require the tag to match the Cargo application version. Every Store
build, including manual dispatch, derives the package version from that same
Cargo version with an appended `.0`. No signing secrets
or Partner Center credentials are required. The release workflow runs the
direct-download and Store builds in parallel, then deploys the website. All must
succeed before it creates the GitHub release.

## Validation and remaining native checks

Run `python3 -m unittest discover -s scripts/tests -p test_build_msix.py -v`
for SDK-free orchestration checks: production-only staging without a private
runtime, feature rejection, automatic version mapping, invalid application versions,
numeric SDK discovery, failure cleanup,
and preservation of existing outputs and caller state. Store-feature Rust
tests also check its storage/profile selection, updater rejection, and native
missing-runtime recovery decisions.
These checks do not substitute for an actual Windows SDK package build.

The Evergreen package was checked natively on Windows on **2026-11-08**: all 84
frontend tests, 11 packaging tests, 47 Store-feature Rust tests, and 45
direct-download Rust tests passed. MakePri and MakeAppx built a validated MSIX
of **2,942,399 bytes**, down from the earlier fixed-runtime package's
322,584,901 bytes. The native smoke check below passed using installed Evergreen
157.0.4341.42, including the frontend, Store update UI, isolated data path,
backend updater rejection, and no GitHub requests or script errors.

Missing-runtime acceptance, cancellation, and browser-launch failure decisions
are covered by Rust tests. Visual verification of the native recovery dialogs
was unavailable because the Windows UI automation helper could not connect.
Local checks cover one development machine, not the complete supported Windows
version range, installed-package behavior, or Store certification.

### Native Store smoke check

Run from the repository root in a Windows SDK shell with `MakeAppx.exe` on
PATH. Set the current Cargo application version for this build, just as tag
builds do in CI. Dirty local version strings skip startup update checks, so
the smoke test requires a release version to cover that path:

```powershell
$env:CANVAZ_VERSION = 'v1.0.0'
py -3 scripts/build-msix.py
npm install --no-save playwright
MakeAppx.exe unpack /p src-tauri/target/release-assets/store/canvaz-store-1.0.0.0-x64.msix /d src-tauri/target/store-smoke /o
node tests/store.windows.mjs src-tauri/target/store-smoke
```

The optional Playwright dependency connects to installed Evergreen WebView2;
no separate Chromium download is needed. Close other Store-edition instances
before running. The check launches the unpacked executable using the normal
Store preference/session paths and a temporary webview profile. It observes a
fresh frontend reload, checks the Store menu and About dialog, exercises the
real backend's updater rejection, and fails on GitHub requests, script errors,
or a private runtime directory in the unpacked layout. It closes its own process
and removes its temporary webview profile. Port 9227 must be available.

This checks the unpacked executable, not Windows package identity,
installation, or Store-delivered updates. Remaining release checks:

- Run the Store workflow to confirm the package also builds on CI.
- Verify an installed Store package launches with Evergreen on a clean machine.
  In a test VM without WebView2, verify the native download prompt, cancellation,
  browser-open failure instructions, and successful launch after installing the
  Evergreen Runtime. Do not uninstall the runtime on a normal development machine.
- Verify Kvaser/PCAN DLL loading and externally installed driver prerequisites
  under package identity. The app package does not install Windows drivers.
- Run the Windows App Certification Kit and native installation, coexistence,
  update, CAN I/O, and data-preservation checks before submission.

Suggested certification notes: Canvaz is a desktop CAN analyzer using native
vendor APIs, local DBC/project files, file dialogs, recording, and export.
`runFullTrust` is needed for that existing desktop functionality. CAN access
requires supported hardware and separately installed vendor drivers. Store
builds leave application updates to Microsoft Store and Windows. Supply exact
reviewer steps and describe the hardware prerequisites in the listing.

References: [MSIX name reservation](https://learn.microsoft.com/en-us/windows/apps/publish/publish-your-app/msix/reserve-your-apps-name),
[product identity details](https://learn.microsoft.com/en-us/windows/apps/publish/view-app-identity-details),
[manual desktop manifest](https://learn.microsoft.com/en-us/windows/msix/desktop/desktop-to-uwp-manual-conversion),
and [Store signing/version requirements](https://learn.microsoft.com/en-us/windows/apps/publish/publish-your-app/msix/app-package-requirements).
