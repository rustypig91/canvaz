import assert from "node:assert/strict";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { chromium } from "playwright";

// Run after npm run build. Install Playwright without changing the lockfile:
// npm install --no-save --package-lock=false playwright
// npx playwright install chromium
// node tests/accessibility.browser.mjs
// Tauri IPC is mocked; exercise real pointer and keyboard menu focus changes.
const server = createServer(async (req, res) => {
    try {
        const path = req.url === "/" ? "index.html" : req.url.slice(1);
        const body = await readFile(new URL(`../dist/${path}`, import.meta.url));
        res.setHeader("Content-Type", path.endsWith(".js") ? "text/javascript" : path.endsWith(".css") ? "text/css" : "text/html");
        res.end(body);
    } catch { res.writeHead(404).end(); }
});
await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
let browser;
try {
    browser = await chromium.launch({ headless: true });
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await page.addInitScript(() => {
        window.__TAURI_INTERNALS__ = {
            metadata: { currentWindow: { label: "main" } },
            transformCallback: () => 1,
            invoke: async command => {
                if (command === "recording_status") return { active: false, path: "", frames: 0, bytes: 0, error: null };
                if (command === "plugin:dialog|save") { window.recordingSaveOpened = true; return null; }
                if (["get_logs", "get_signal_history", "get_frames", "get_bus_stats"].includes(command)) return [];
                if (command === "get_version") return "dev";
                if (command === "get_app_data_dir") return "/mock";
                if (command === "read_text_file" || command === "load_project") throw new Error("No saved data");
                return null;
            },
        };
    });
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    await page.waitForSelector(".plot-pane", { state: "attached" });
    await page.click("#menu-data .menu-trigger");
    await page.click('[data-action="log-to-disk"]');
    await page.waitForFunction(() => window.recordingSaveOpened === true, null, {timeout: 3000});
    await page.click("#menu-data .menu-trigger");
    await page.keyboard.press("ArrowDown");
    assert.equal(await page.locator("#menu-data").evaluate(el => el.classList.contains("open")), true);
    await page.locator("#btn-show-log").focus();
    assert.equal(await page.locator("#menu-data").evaluate(el => el.classList.contains("open")), false);
    assert.deepEqual(errors, []);
    console.log("Recording menu click opens save dialog");
} finally {
    await browser?.close();
    await new Promise(resolve => server.close(resolve));
}
