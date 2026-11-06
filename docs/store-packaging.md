# Microsoft Store packaging and identity

## Selected route

Use an **x64 MSIX** containing the production Tauri/Win32 application. Register
it in Partner Center as an **MSIX or PWA app**. The existing NSIS/MSI installers
remain the direct-download route. `scripts/build-msix.py` builds the production
frontend and Store-feature executable, stages the package and bundled WebView2,
indexes scaled icons with MakePri, and runs MakeAppx with validation enabled.

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

Start the Store package at **1.0.0.0**, maintained explicitly as `packageVersion`
in the identity JSON. The application remains version **0.6.4**; this work does
not bump the app or rename its release tag. Store package versioning is separate
because the first component cannot be zero and the fourth is reserved for Store
use and must be zero. Each component is at most 65535.

Increase the package version for each new Store package intended to upgrade
installed copies, including packaging-only revisions. Do not derive it blindly
from the current `0.x` Cargo version. A command-line override is available when
preparing a new packaging revision; it does not change the app's displayed
version. Keep a record of the version submitted to Partner Center.

## Validate and prepare the manifest

With the supplied identity configured, run from the repository root:

```sh
python3 scripts/prepare-store-manifest.py --check
python3 scripts/prepare-store-manifest.py --output src-tauri/target/store/layout/AppxManifest.xml
# Example packaging-only revision:
python3 scripts/prepare-store-manifest.py --package-version 1.0.1.0 --output src-tauri/target/store/layout/AppxManifest.xml
```

The script uses only Python's standard library and works from any working
directory. It checks required identity values, rejects template placeholders,
enforces the Store version format, and escapes XML values. It cannot verify
name availability, account ownership, or whether the values match Partner Center.
It does not create a product, build an executable, sign, package, upload, or
publish anything. The template is not a submission-ready manifest on its own.

## Dedicated Windows build

Install Python 3.11+, Node.js, Rust's Windows x64 MSVC toolchain, Visual Studio
C++ Build Tools, and the Windows 10/11 SDK (MakePri, MakeAppx, and expand.exe).
From the repository root in PowerShell:

```powershell
py -3 scripts/build-msix.py
# Reuse node_modules, but always rebuild the frontend and Rust executable:
py -3 scripts/build-msix.py --skip-dependency-install
# Packaging-only revision or alternate output directory:
py -3 scripts/build-msix.py --package-version 1.0.1.0 --output-directory release-assets/store
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
application/package versions, active features, Store identity, and bundled
runtime version/source/hash. The MSIX is unsigned for Partner Center ingestion;
it is not a public unsigned sideload installer. This script does not install,
upload, or publish the package.

## Bundled WebView2

`packaging/windows/msix/webview2-runtime.json` pins Microsoft's x64 Fixed Version
runtime, currently **157.0.4341.42**, with its official download URL and SHA-256.
The builder verifies cached and downloaded CABs, extracts them with expand.exe,
checks the runtime is an x64 PE executable, and includes its full directory as
`WebView2/` beside `canvaz.exe`. The Store context selects that directory before
Tauri creates its webview; it does not depend on a machine-wide Evergreen
installation. Microsoft runtime license/resource files remain in the bundle.

The CAB is about 311 MB and the extracted runtime is larger, so expect a larger
Store package than the standalone executable. Verified CABs are cached under
`src-tauri/target/store-downloads` for later builds. Corrupt cache entries are
not trusted or reused.

Fixed Version runtimes do not update themselves. Before Store releases, review
[Microsoft's runtime downloads](https://developer.microsoft.com/en-us/microsoft-edge/webview2/),
update the pinned version/URL/hash when needed, and increase the Store package
version. Customers receive runtime updates in the next Store package. See
[Microsoft's distribution guidance](https://learn.microsoft.com/en-us/microsoft-edge/webview2/concepts/distribution).

## GitHub Actions

`.github/workflows/store.yml` runs on relevant PRs, all `main` pushes, `v*` tags,
and manual dispatch. Only superseded PR runs are cancelled. It tests the
frontend, packaging orchestration, identity, and Store-feature Rust code, then
uploads `canvaz-store-msix-<run-id>` with the MSIX, checksum, and build metadata.
Artifact replacement supports reruns. Download this artifact for submission;
the workflow does not publish it to GitHub Releases or Partner Center.

Tag builds require the tag to match the Cargo application version. The separate
Store package version still comes from `store-identity.json`; manual dispatch
offers an optional override for a packaging-only revision. No signing secrets
or Partner Center credentials are required. Direct-download release builds
continue through the existing `build` workflow independently.

## Validation and remaining native checks

Run `python3 -m unittest discover -s scripts/tests -p test_build_msix.py -v`
for SDK-free orchestration checks: production-only staging, runtime integrity,
feature rejection, version overrides, numeric SDK discovery, failure cleanup,
and preservation of existing outputs and caller state. Store-feature Rust
tests also check its storage/profile selection and updater rejection.
These checks do not substitute for an actual Windows SDK package build.

- Run the Store workflow to verify the real MakePri/MakeAppx output and inspect
  native DLL dependencies on Windows.
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
