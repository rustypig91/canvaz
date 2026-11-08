# Canvaz Microsoft Store submission materials

This archive is for the maintainer submitting Canvaz in Partner Center.
The MSIX is unsigned; Microsoft signs it during Store publication. Use the
EXE/MSI release assets for direct installation.

## Archive contents

- `package/`: the unsigned MSIX, its SHA-256 checksum, and build metadata.
- `packaging/windows/store-listing/`: all seven listing image exports, including
  app tiles (71, 150, and 300 pixels), box art (1080 and 2160 pixels square), and
  poster art (720 x 1080 and 1440 x 2160 pixels).
- `packaging/windows/msix/Assets/`: all six package icons, including 200% scale
  variants. These are already embedded in the MSIX; separate listing images
  belong in the Store listing fields.
- `screenshots/canvaz-linux-demo.png`: the automated 1600 x 900 Linux demo
  screenshot from the same release build. Review it as a reference and capture
  Windows Store edition screenshots for the final listing.
- `packaging/windows/msix/store-identity.json` and `AppxManifest.xml.in`:
  package identity and manifest template for reference.
- `APPLICATION.md`, `docs/`, `packaging/windows/README.md`, and `LICENSE`:
  application overview, Store packaging/data behavior, privacy policy, artwork
  dimensions, and license.

## Submit in Partner Center

1. Open the existing Canvaz product in
   [Partner Center](https://partner.microsoft.com/dashboard/apps-and-games).
   Confirm its identity matches `packaging/windows/msix/store-identity.json`.
2. Upload the `.msix` from `package/` in the submission's Packages section.
   The ZIP, checksum, and build metadata are reference materials, not app packages.
3. Add the description, features, and version notes in the Store listing.
   `APPLICATION.md` contains the application overview; review the listing copy
   and complete the category, age ratings, pricing, and other required fields.
4. Upload the listing images to the corresponding fields. Include screenshots
   showing the Windows app and review their captions before submitting.
5. Verify the deployed privacy policy URL before entering it in Partner Center:
   <https://rustypig91.github.io/canvaz/privacy/>. The policy source is included
   under `docs/site/privacy/index.html`.
6. Complete the native Windows checks described in `docs/store-packaging.md`
   and review the submission before sending it for certification.

Microsoft's [submission guidance](https://learn.microsoft.com/en-us/windows/apps/publish/publish-your-app/msix/create-app-submission)
and [image guidance](https://learn.microsoft.com/en-us/windows/apps/publish/publish-your-app/msix/screenshots-and-images)
describe the available fields and current requirements.
