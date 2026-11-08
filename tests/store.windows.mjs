import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { chromium } from "playwright";

// Run against an unpacked production MSIX, not an installed user application:
// node tests/store.windows.mjs path/to/unpacked-msix
// Requires the optional Playwright dependency; see docs/store-packaging.md.
assert.equal(process.platform, "win32", "This check requires native Windows WebView2.");
assert.ok(process.argv[2], "Pass the unpacked MSIX directory.");
const layout = resolve(process.argv[2]);
assert.ok(existsSync(join(layout, "canvaz.exe")), "Missing Store executable.");
assert.equal(existsSync(join(layout, "WebView2")), false, "Store package must use shared Evergreen WebView2.");
const endpoint = "http://127.0.0.1:9227";
assert.equal(await fetch(`${endpoint}/json/version`).then(() => true, () => false), false,
    "Test debugging port 9227 must be unused.");

const temporaryRoot = resolve(tmpdir());
const profile = mkdtempSync(join(temporaryRoot, "canvaz-store-webview-"));
const app = spawn(join(layout, "canvaz.exe"), [], {
    cwd: layout, windowsHide: true,
    env: {
        ...process.env,
        // An inherited development override must not select a private runtime.
        WEBVIEW2_BROWSER_EXECUTABLE_FOLDER: "",
        WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: "--remote-debugging-port=9227",
        WEBVIEW2_USER_DATA_FOLDER: profile,
    },
    stdio: ["ignore", "pipe", "pipe"],
});
let output = "", browser, page, result;
app.stdout.on("data", data => { output += data; });
app.stderr.on("data", data => { output += data; });
app.on("error", error => { output += String(error); });

try {
    for (let attempt = 0; attempt < 60; attempt++) {
        assert.equal(app.exitCode, null, `Store app exited: ${output}`);
        if (await fetch(`${endpoint}/json/version`).then(response => response.ok, () => false)) break;
        await delay(500);
    }
    browser = await chromium.connectOverCDP(endpoint);
    const context = browser.contexts()[0];
    for (let attempt = 0; attempt < 40; attempt++) {
        page = context.pages().find(candidate => candidate.url().includes("tauri.localhost"));
        if (page) break;
        await delay(250);
    }
    assert.ok(page, `Embedded frontend did not load: ${output}`);
    const requests = [], errors = [];
    page.on("request", request => {
        if (/github\.com|githubusercontent\.com/.test(request.url())) requests.push(request.url());
    });
    page.on("pageerror", error => errors.push(error.message));
    // Reload after attaching so startup requests and script failures are observed.
    await page.reload();
    await page.locator(".plot-pane").waitFor({ state: "attached", timeout: 15000 });
    const appVersion = await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("get_version"));
    assert.match(appVersion, /^v\d+\.\d+\.\d+$/,
        "Build with CANVAZ_VERSION=v<app-version> to exercise release startup update checks.");
    assert.equal(await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("update_channel")), "microsoft-store");
    assert.equal(await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("demo_scene")), null);
    const dataPath = await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("get_app_data_dir"));
    assert.ok(dataPath.endsWith("com.christoffer.canvaz.store"), dataPath);
    for (const [command, args] of [["update_support", {}], ["download_update", { version: "v999.0.0" }], ["install_update", {}]]) {
        const error = await page.evaluate(async ([command, args]) => {
            try { await window.__TAURI_INTERNALS__.invoke(command, args); return null; }
            catch (error) { return String(error); }
        }, [command, args]);
        assert.match(error, /Microsoft Store manages updates/);
    }
    await page.locator("#menu-about .menu-trigger").click();
    const updates = page.locator('[data-action="check-updates"]');
    assert.equal(await updates.textContent(), "Microsoft Store updates…");
    assert.equal(await updates.isEnabled(), true);
    await updates.click();
    await page.locator("#dialog-update[open]").waitFor();
    assert.equal(await page.locator("#btn-update-manual").textContent(), "Open Microsoft Store");
    assert.equal(await page.locator("#btn-update-download").isVisible(), false);
    assert.equal(await page.locator("#btn-update-skip").isVisible(), false);
    if (process.env.STORE_SMOKE_SCREENSHOT) {
        await page.screenshot({ path: process.env.STORE_SMOKE_SCREENSHOT, animations: "disabled" });
    }
    await page.keyboard.press("Escape");
    await page.locator("#menu-about .menu-trigger").click();
    await page.locator('[data-action="about"]').click();
    await page.locator("#about-update-channel").waitFor({ state: "visible" });
    assert.match(await page.locator("#about-update-channel").textContent(), /Microsoft Store manages updates/);
    await delay(1000);
    assert.deepEqual(requests, [], "Store startup and manual updates must not contact GitHub.");
    assert.deepEqual(errors, [], "No script errors in the real WebView2 runtime.");
    assert.match(await browser.version(), /^\d+\.\d+\.\d+\.\d+$/, "Evergreen WebView2 must report its version.");
    result = { nativeStoreSmoke: "passed", appVersion, url: page.url(), dataPath,
        browserVersion: await browser.version(), githubRequests: requests, errors };
} finally {
    // Close only the process started by this check; leave other app instances alone.
    await page?.evaluate(() => window.__TAURI_INTERNALS__.invoke("plugin:window|close", { label: "main" })).catch(() => {});
    for (let attempt = 0; app.exitCode === null && attempt < 30; attempt++) await delay(100);
    await browser?.close().catch(() => {});
    if (app.exitCode === null) app.kill();
    assert.equal(dirname(profile), temporaryRoot);
    // WebView2 can release its profile locks after the owning process exits.
    // Yield between attempts so a transient Windows sharing violation is retried.
    for (let attempt = 0; ; attempt++) {
        try { rmSync(profile, { recursive: true, force: true }); break; }
        catch (error) {
            if (attempt >= 39 || !["EPERM", "EBUSY", "ENOTEMPTY"].includes(error.code)) throw error;
            await delay(250);
        }
    }
    if (output) console.log(output);
}
console.log(JSON.stringify(result, null, 2));
