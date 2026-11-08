import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

const source = ts.createSourceFile("main.ts", readFileSync(new URL("../src/main.ts", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true);
const names = ["initializeUpdates", "fetchLatestRelease", "openUpdateDialog", "checkForUpdates", "performUpdateCheck", "isNewerVersion"];
const code = ts.transpileModule(source.statements.filter(n => ts.isFunctionDeclaration(n) && names.includes(n.name?.text)).map(n => n.getText(source)).join("\n"), {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.None },
}).outputText;

function harness(channel, { policyError = false, storeError = false } = {}) {
    const elements = new Map();
    const action = {};
    const calls = [], requests = [], opened = [];
    function element(id) {
        if (!elements.has(id)) elements.set(id, {
            style: {}, disabled: false, hidden: false, textContent: "", open: false,
            showModal() { this.open = true; }, close() { this.open = false; },
        });
        return elements.get(id);
    }
    const context = vm.createContext({
        updateChannel: "disabled", demoMode: false, updateBusy: false, updateChecking: false,
        currentVersion: "v0.6.4", preferences: { skippedVersion: "v0.6.5" },
        RELEASES_LATEST_API: "https://api.github.com/repos/rustypig91/canvaz/releases/latest",
        RELEASES_PAGE_URL: "https://github.com/rustypig91/canvaz/releases/latest",
        AbortSignal, autoSaveTimer: null, sessionFilePath: "/mock/last-session.canvaz",
        document: { getElementById: element, querySelector: () => action },
        invoke: async (command, args) => {
            calls.push([command, args]);
            if (command === "update_channel") {
                if (policyError) throw new Error("IPC unavailable");
                return channel;
            }
            if (command === "open_store_updates" && storeError) throw new Error("Store is unavailable");
            return null;
        },
        fetch: async url => {
            requests.push(url);
            return { ok: true, json: async () => ({ tag_name: "v0.6.5", html_url: "https://github.com/rustypig91/canvaz/releases/tag/v0.6.5" }) };
        },
        openUrl: async url => opened.push(url),
        listen: async () => () => {}, buildProject: () => ({}),
        log() {}, savePreferences: () => calls.push(["savePreferences"]),
    });
    vm.runInContext(code, context);
    return { context, action, element, calls, requests, opened };
}

test("Store startup skips GitHub even with an imported skipped-version preference", async () => {
    const h = harness("microsoft-store");
    await h.context.initializeUpdates();
    await h.context.checkForUpdates(false);
    assert.equal(h.requests.length, 0);
    assert.equal(h.element("dialog-update").open, false);
    assert.equal(h.action.textContent, "Microsoft Store updates…");
    assert.equal(h.action.disabled, false);
    assert.equal(h.element("about-update-channel").hidden, false);
    assert.match(h.element("about-update-channel").textContent, /Microsoft Store manages updates/);
});

test("Store manual action explains updates and opens only the fixed backend Store command", async () => {
    const h = harness("microsoft-store");
    await h.context.initializeUpdates();
    await h.context.checkForUpdates(true);
    assert.equal(h.element("dialog-update").open, true);
    assert.equal(h.element("btn-update-download").style.display, "none");
    assert.equal(h.element("btn-update-skip").style.display, "none");
    assert.equal(h.element("btn-update-manual").textContent, "Open Microsoft Store");
    await h.element("btn-update-manual").onclick();
    // Even invoking hidden/stale controls cannot install or change preferences.
    await h.element("btn-update-download").onclick();
    h.element("btn-update-skip").onclick();
    assert.deepEqual(h.calls.map(([command]) => command), ["update_channel", "open_store_updates"]);
    assert.deepEqual(h.requests, []);
    assert.deepEqual(h.opened, []);
});

test("Store ignores stale GitHub dialog options and guards lower-level check entry points", async () => {
    const h = harness("microsoft-store");
    await h.context.initializeUpdates();
    h.context.openUpdateDialog({ title: "GitHub update", message: "Upgrade now", downloadUrl: "https://github.com/download", skipVersion: "v99.0.0" });
    assert.equal(h.element("update-title").textContent, "Microsoft Store updates");
    assert.equal(h.element("btn-update-download").style.display, "none");
    await h.context.performUpdateCheck(true);
    await assert.rejects(h.context.fetchLatestRelease(), /GitHub updates are unavailable/);
    assert.deepEqual(h.requests, []);
});

test("failure to open Microsoft Store provides manual instructions without GitHub fallback", async () => {
    const h = harness("microsoft-store", { storeError: true });
    await h.context.initializeUpdates();
    await h.context.checkForUpdates(true);
    await h.element("btn-update-manual").onclick();
    assert.match(h.element("update-message").textContent, /Open Microsoft Store manually/);
    assert.deepEqual(h.requests, []);
    assert.deepEqual(h.opened, []);
});

for (const [label, channel, options] of [
    ["demo", "disabled", {}],
    ["unknown channel", "unexpected", {}],
    ["missing backend response", null, {}],
    ["failed backend response", null, { policyError: true }],
]) {
    test(`${label} fails closed before any GitHub request`, async () => {
        const h = harness(channel, options);
        await h.context.initializeUpdates();
        await h.context.checkForUpdates(false);
        await h.context.checkForUpdates(true);
        await h.context.performUpdateCheck(true);
        await assert.rejects(h.context.fetchLatestRelease(), /GitHub updates are unavailable/);
        assert.equal(h.action.disabled, true);
        assert.equal(h.element("dialog-update").open, false);
        assert.deepEqual(h.requests, []);
    });
}

test("direct downloads retain startup checks, skipped notices, and manual installation", async () => {
    const h = harness("github");
    await h.context.initializeUpdates();
    assert.equal(h.action.textContent, "Check for Updates…");
    assert.equal(h.element("about-update-channel").hidden, true);
    await h.context.checkForUpdates(false);
    assert.equal(h.requests.length, 1);
    assert.equal(h.element("dialog-update").open, false);
    await h.context.checkForUpdates(true);
    assert.equal(h.requests.length, 2);
    assert.equal(h.element("dialog-update").open, true);
    assert.equal(h.element("btn-update-download").style.display, "");
    assert.equal(h.element("btn-update-manual").textContent, "Downloads page");
    await h.element("btn-update-download").onclick();
    assert.deepEqual(h.calls.slice(-3).map(([command]) => command), ["download_update", "save_project", "install_update"]);
});
