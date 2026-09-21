import assert from "node:assert/strict";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { chromium } from "playwright";

// Run after npm run build. Install Playwright without changing the lockfile:
// npm install --no-save --package-lock=false playwright
// npx playwright install chromium
// node tests/accessibility.browser.mjs
// Tauri IPC is mocked; every UI action below uses the keyboard.
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
        const signal = { name: "Speed", message_id: 123, message_name: "Engine", start_bit: 0, length: 8,
            little_endian: true, signed: false, factor: 1, offset: 0, min: 0, max: 255, unit: "km/h", enum_values: [] };
        window.__TAURI_INTERNALS__ = {
            metadata: { currentWindow: { label: "main" } },
            transformCallback: () => 1,
            invoke: async command => {
                if (command === "list_can_interfaces") return [{ name: "vcan0", backend: "socketcan" }];
                if (command === "create_channel") return { handle: 1, backend: "socketcan", available: true };
                if (command === "plugin:dialog|open") return "keyboard.dbc";
                if (command === "parse_dbc") return { nodes: ["ECU"], messages: { 123: { id: 123, key: 123, name: "Engine", transmitter: "ECU", length: 8, signals: [signal] } } };
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
    async function tabTo(selector) {
        for (let n = 0; n < 150; n++) {
            if (await page.evaluate(s => document.activeElement?.matches(s), selector)) return;
            await page.keyboard.press("Tab");
        }
        throw new Error(`Cannot reach ${selector} with Tab`);
    }
    async function focused(selector) {
        try { await page.waitForFunction(s => document.activeElement?.matches(s), selector, { timeout: 3000 }); }
        catch (error) { console.error(await page.evaluate(() => ({ active: document.activeElement?.outerHTML, tree: document.getElementById("dbc-tree")?.innerHTML }))); throw error; }
    }
    await tabTo("#btn-add-channel");
    await page.keyboard.press("Enter");
    await focused("#select-iface");
    await tabTo("#input-channel-name");
    await page.keyboard.type("Keyboard CAN");
    await tabTo("#btn-browse-dbc");
    await page.keyboard.press("Enter");
    await tabTo("#btn-channel-apply");
    await page.keyboard.press("Enter");
    await focused("#btn-add-channel");
    await page.waitForSelector(".msg-group");
    assert.match(await page.locator(".ch-name").textContent(), /Keyboard CAN/);
    await tabTo(".btn-edit-ch");
    await page.keyboard.press("Enter");
    await page.waitForSelector("#dialog-channel[open]");
    await page.keyboard.press("Escape");
    await focused(".btn-edit-ch");
    await tabTo(".ecu-summary");
    await page.keyboard.press("ArrowLeft");
    assert.equal(await page.locator(".ecu-group").evaluate(el => el.open), false);
    await page.keyboard.press("ArrowRight");
    await page.keyboard.press("ArrowDown");
    await focused(".msg-group > summary");
    await page.keyboard.press("ArrowRight");
    await page.waitForSelector(".signal-row");
    await page.keyboard.press("ArrowDown");
    await focused(".signal-row");
    await page.keyboard.press("p");
    await page.keyboard.press("s");
    assert.equal(await page.locator("#view-trace").getAttribute("aria-selected"), "true");
    await tabTo("#view-trace");
    await page.keyboard.press("ArrowRight");
    await tabTo(".signal-row");
    await page.keyboard.press("Enter");
    await page.waitForSelector(".signal-row.in-plot");
    assert.equal(await page.locator("#view-plot").getAttribute("aria-selected"), "true");
    await focused(".signal-row");
    await tabTo("#view-plot");
    await page.keyboard.press("ArrowRight");
    await tabTo(".signal-row");
    await page.keyboard.press("Enter");
    await page.waitForSelector(".sim-msg-name");
    assert.equal(await page.locator(".sim-msg-name").textContent(), "Engine");
    assert.equal(await page.locator("#view-simulate").getAttribute("aria-selected"), "true");
    await tabTo("#view-simulate");
    await page.keyboard.press("Home");
    await focused("#view-trace");
    assert.equal(await page.locator("#view-trace").getAttribute("aria-selected"), "true");
    await tabTo(".trace-filter-button");
    await page.keyboard.press("Enter");
    await page.waitForSelector('.ctx-menu[role="dialog"]');
    await page.keyboard.press("Escape");
    await focused(".trace-filter-button");
    await tabTo("#menu-about .menu-trigger");
    await page.keyboard.press("ArrowDown");
    await page.keyboard.press("End");
    await page.keyboard.press("Enter");
    await page.waitForSelector("#dialog-about[open]");
    await page.keyboard.press("Escape");
    await focused("#menu-about .menu-trigger");
    assert.equal(await page.locator("#menu-about .menu-trigger").getAttribute("aria-expanded"), "false");
    assert.deepEqual(errors, []);
    console.log("PASS: keyboard add/edit channel, DBC navigation, plot signal, simulate message, tabs, trace filters, menus and dialog focus restoration");
} finally {
    await browser?.close();
    await new Promise(resolve => server.close(resolve));
}
