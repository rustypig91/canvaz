# Privacy policy publication

The public policy source is `docs/site/privacy/index.html`. Review it against
application behavior and update its visible date whenever its content changes.
The contact address follows the existing Pigtail policy for the same developer.

The intended Partner Center URL is:

```text
https://rustypig91.github.io/canvaz/privacy/
```

This URL is not live until the site has been successfully deployed.

## First publication

1. Merge the policy and workflow into `main`.
2. In repository **Settings → Pages**, set the build source to **GitHub Actions**.
3. Ensure the **github-pages** environment allows deployments from `main` and
   release tags matching `v*`. A branch-only restriction will block tag releases.
4. Run **Actions → Publish privacy policy → Run workflow** from `main`.
5. Open the URL printed in the workflow summary and verify it before entering
   it in Partner Center. A custom domain changes the URL; use the deployed one.

The existing release workflow also calls the reusable privacy workflow after
its release job succeeds. It publishes the policy from the released commit,
alongside APT publication. PRs, ordinary pushes, and failed releases do not
deploy the site. Manual dispatch supports policy-only publication.

The workflow uploads only `docs/site`, uses the standard GitHub Pages token
permissions, and requires no custom token or domain. Deployments serialize
without cancelling an in-progress publication. Artifact names include the run
and attempt; deploy-only retries reuse the successful build job's artifact
within its retention period.

Pages deployment replaces the repository's entire Pages site. Keep any future
public pages under `docs/site` so they are included in the deployment.

## Before the Store release

The policy describes the separate Store storage and Store-managed update
behavior implemented by the `microsoft-store` feature. Native Windows checks
must still verify package virtualization, Store launch, and the absence of
GitHub update requests/notices in the installed Store edition. Implementation
and validation details are in [Store and direct-download data](store-data.md).
Do not copy Pigtail's opt-out update setting or capture cleanup claims: Canvaz
does not currently provide those behaviors.

For a local preview, run `python3 -m http.server 8000 --directory docs/site` and
open `http://localhost:8000/privacy/`. Check both color schemes and a narrow
viewport after layout changes. The page has no external scripts or assets.

Reference: [GitHub Pages custom workflows](https://docs.github.com/en/pages/getting-started-with-github-pages/using-custom-workflows-with-github-pages).
