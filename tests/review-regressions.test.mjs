import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

const source = ts.createSourceFile("main.ts", readFileSync(new URL("../src/main.ts", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true);
function harness(names, globals = {}) {
    const functions = source.statements.filter(n => ts.isFunctionDeclaration(n) && names.includes(n.name?.text));
    assert.equal(functions.length, names.length);
    const code = ts.transpileModule(functions.map(n => n.getText(source)).join("\n").replaceAll("import.meta.env.DEV", "false"), {
        compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.None },
    }).outputText;
    const context = vm.createContext({ log() {}, ...globals });
    vm.runInContext(code, context);
    return context;
}
function deferred() {
    let resolve, reject;
    const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
    return { promise, resolve, reject };
}
function simHarness(invoke) {
    const entry = { kind: "raw", channel: 1, canId: 0x123, isExtended: false, dlc: 1, data: [0], periodMs: 100, running: false, periodicHandle: null };
    const ctx = harness(["startSim", "stopSim", "queueSimOperation", "removeSimEntry", "updateRunningSim"], {
        invoke, simEntries: new Map([["raw", entry]]), channels: new Map([[1, { open: true }]]), appRunning: true,
        document: { querySelector: () => null }, scheduleAutoSave() {}, updateSimTxStatus() {}, updateSignalHighlights() {},
    });
    return { ctx, entry };
}

test("Stop waits for delayed Start and removes its returned handle", async () => {
    const added = deferred(), entered = deferred(), removed = [];
    const { ctx, entry } = simHarness(async (command, args) => {
        if (command === "add_periodic_frame") { entered.resolve(); return added.promise; }
        assert.equal(command, "remove_periodic");
        removed.push(args.cmd.periodic_handle);
    });
    const starting = ctx.startSim("raw");
    await entered.promise;
    const stopping = ctx.stopSim("raw");
    added.resolve(42);
    await Promise.all([starting, stopping]);
    assert.deepEqual(removed, [42]);
    assert.equal(entry.running, false);
    assert.equal(entry.periodicHandle, null);
});

test("removing an entry during Start cleans up the backend periodic", async () => {
    const added = deferred(), entered = deferred(), calls = [];
    const { ctx } = simHarness(async command => {
        calls.push(command);
        if (command === "add_periodic_frame") { entered.resolve(); return added.promise; }
    });
    const starting = ctx.startSim("raw");
    await entered.promise;
    const removing = ctx.removeSimEntry("raw");
    added.resolve(9);
    await Promise.all([starting, removing]);
    assert.deepEqual(calls, ["add_periodic_frame", "remove_periodic"]);
    assert.equal(ctx.simEntries.size, 0);
});

test("failed Stop retains the handle and allows retry", async () => {
    let fail = true;
    const { ctx, entry } = simHarness(async command => {
        if (command === "add_periodic_frame") return 42;
        if (fail) throw new Error("temporary failure");
    });
    await ctx.startSim("raw");
    await assert.rejects(ctx.stopSim("raw"));
    assert.equal(entry.running, true);
    assert.equal(entry.periodicHandle, 42);
    fail = false;
    await ctx.stopSim("raw");
    assert.equal(entry.periodicHandle, null);
});

test("Start/Stop/Start keeps exactly one registered periodic", async () => {
    const added = deferred(), entered = deferred(), active = new Set();
    let next = 0;
    const { ctx, entry } = simHarness(async (command, args) => {
        if (command === "remove_periodic") { active.delete(args.cmd.periodic_handle); return; }
        const handle = ++next;
        if (handle === 1) { entered.resolve(); await added.promise; }
        active.add(handle);
        return handle;
    });
    const first = ctx.startSim("raw");
    await entered.promise;
    const stop = ctx.stopSim("raw"), second = ctx.startSim("raw");
    added.resolve();
    await Promise.all([first, stop, second]);
    assert.deepEqual([...active], [2]);
    assert.equal(entry.periodicHandle, 2);
    assert.equal(entry.running, true);
});

test("a failed in-flight update cannot restart an entry after Stop", async () => {
    const update = deferred(), entered = deferred(), calls = [];
    const { ctx, entry } = simHarness(async command => {
        calls.push(command);
        if (command === "add_periodic_frame") return 1;
        if (command === "update_periodic_frame") { entered.resolve(); return update.promise; }
    });
    await ctx.startSim("raw");
    const updating = ctx.updateRunningSim("raw");
    await entered.promise;
    const stopping = ctx.stopSim("raw");
    update.reject(new Error("update failed"));
    await Promise.all([updating, stopping]);
    assert.deepEqual(calls, ["add_periodic_frame", "update_periodic_frame", "remove_periodic"]);
    assert.equal(entry.running, false);
    assert.equal(entry.periodicHandle, null);
});

test("failed Save As preserves the old path and unsaved state", async () => {
    const ctx = harness(["saveProjectAs", "ensureCanvazExt"], {
        dialogSave: async () => "new.canvaz", projectPath: "old.canvaz", projectDirty: true, projectRevision: 0,
        buildProject: () => ({}), updateWindowTitle() {}, persistLastProjectPath: () => assert.fail("must not persist a failed save"),
        invoke: async () => { throw new Error("disk full"); },
    });
    await ctx.saveProjectAs();
    assert.equal(ctx.projectPath, "old.canvaz");
    assert.equal(ctx.projectDirty, true);
});

test("edits made during Save As remain dirty after the write completes", async () => {
    const write = deferred(), entered = deferred();
    let value = 1;
    const ctx = harness(["saveProjectAs", "ensureCanvazExt"], {
        dialogSave: async () => "new.canvaz", projectPath: "old.canvaz", projectDirty: true, projectRevision: 0,
        buildProject: () => ({ value }), updateWindowTitle() {}, persistLastProjectPath() {}, invoke: () => { entered.resolve(); return write.promise; },
    });
    const saving = ctx.saveProjectAs();
    await entered.promise;
    value = 2;
    write.resolve();
    await saving;
    assert.equal(ctx.projectPath, "new.canvaz");
    assert.equal(ctx.projectDirty, true);
});

test("canceling Open Project preserves unsaved work and project identity", async () => {
    let prompted = false;
    const ctx = harness(["openProject"], {
        dialogOpen: async () => "other.canvaz", invoke: async () => ({}), projectDirty: true, projectPath: "current.canvaz",
        showConfirm: async () => { prompted = true; return false; },
        applyProject: () => assert.fail("must not replace the project"),
    });
    await ctx.openProject();
    assert.equal(prompted, true);
    assert.equal(ctx.projectPath, "current.canvaz");
    assert.equal(ctx.projectDirty, true);
});

test("New Project keeps session saving and dirty tracking active for subsequent edits", async () => {
    const writes = [], timers = new Map();
    let nextTimer = 0, value = 0;
    const ctx = harness(["newProject", "scheduleAutoSave"], {
        sessionFilePath: "session.canvaz", projectPath: "old.canvaz", projectDirty: false, projectRevision: 0, restoringProject: false,
        autoSaveTimer: null, appRunning: false, channels: new Map(), ghostChannels: [], plotPanes: [], pendingPaneSignals: [], pendingSimMessages: [], simEntries: new Map(),
        signalLastValues: new Map(), signalLastRaw: new Map(), signalMinValues: new Map(), signalMaxValues: new Map(),
        document: { getElementById: () => ({ innerHTML: "" }) },
        clearTrace() {}, updateWindowTitle() {}, refreshChannelList() {}, rebuildTraceColumns() {}, renderDbcTree() {},
        persistLastProjectPath: path => assert.equal(path, ""), buildProject: () => ({ value }),
        setTimeout: fn => { timers.set(++nextTimer, fn); return nextTimer; }, clearTimeout: id => timers.delete(id),
        invoke: async (command, args) => { writes.push({ command, ...args }); },
    });
    await ctx.newProject();
    assert.equal(ctx.sessionFilePath, "session.canvaz");
    assert.equal(ctx.projectPath, null);
    value = 2;
    ctx.scheduleAutoSave("edit after New");
    assert.equal(ctx.projectDirty, true);
    assert.equal(timers.size, 1);
    await [...timers.values()][0]();
    assert.equal(writes[0].path, "session.canvaz");
    assert.equal(writes[0].project.value, 2);
});

test("standard and extended IDs have separate trace, decoding and plot identities", () => {
    const std = { id: 0x123, key: 0x123, is_extended: false, signals: [{ name: "Value" }] };
    const ext = { id: 0x123, key: 0x80000123, is_extended: true, signals: [{ name: "Value" }] };
    const ctx = harness(["dbcMessageId", "dbcMessageFor", "traceKey", "sigKeysFor", "plotKey", "savedDbcMessage"], {
        channels: new Map([[1, { dbc: { messages: { [std.key]: std, [ext.key]: ext } }, config: {} }]]), sigKeyCache: new Map(),
    });
    assert.notEqual(ctx.traceKey(1, std.id, "rx", false), ctx.traceKey(1, ext.id, "rx", true));
    assert.equal(ctx.dbcMessageFor(1, 0x123, false), std);
    assert.equal(ctx.dbcMessageFor(1, 0x123, true), ext);
    assert.notEqual(ctx.sigKeysFor(1, std)[0], ctx.sigKeysFor(1, ext)[0]);
    assert.equal(ctx.savedDbcMessage({ messages: { [ext.key]: ext } }, 0x123), ext, "legacy unambiguous references still resolve");
});

test("legacy and new low extended IDs restore plots and simulator references", async () => {
    for (const savedId of [0x123, 0x80000123]) {
        const signal = { name: "Value", message_id: 0x80000123 };
        const message = { id: 0x123, key: 0x80000123, is_extended: true, signals: [signal] };
        const plotted = [];
        const ctx = harness(["restoreProjectEntries", "savedDbcMessage", "dbcMessageId"], {
            channels: new Map([[1, { dbc: { messages: { [message.key]: message } } }]]),
            pendingPaneSignals: [[{ channel: "test", message_id: savedId, signal_name: "Value" }]],
            pendingSimMessages: [{ channel: "test", message_id: savedId, period_ms: 100, signals: [{ name: "Value", value: 5 }] }],
            plotPanes: [{}], simEntries: new Map(), msgEntryCounter: 0, idToHandle: () => 1,
            addSignalToPane: async (_pane, _handle, sig) => plotted.push(sig),
            document: { getElementById: () => ({ appendChild() {} }) }, createSimEntryEl() {}, renderSimEntries() {}, updateSignalHighlights() {},
        });
        await ctx.restoreProjectEntries();
        assert.equal(plotted[0].message_id, message.key);
        assert.equal([...ctx.simEntries.values()][0].messageId, message.key);
        assert.equal(ctx.pendingPaneSignals.length, 0);
        assert.equal(ctx.pendingSimMessages.length, 0);
    }
});

test("raw frame edits and DBC period edits schedule persistence while stopped", async () => {
    class Element {
        dataset = {};
        listeners = new Map();
        children = new Map();
        addEventListener(type, fn) { this.listeners.set(type, fn); }
        querySelector(selector) {
            if (!this.children.has(selector)) this.children.set(selector, new Element());
            return this.children.get(selector);
        }
        querySelectorAll(selector) {
            if (selector === ".sim-byte") {
                const byte = this.querySelector(selector);
                byte.dataset.idx = "0";
                return [byte];
            }
            return [];
        }
        async fire(type, value, checked = false) {
            this.value = value;
            this.checked = checked;
            await this.listeners.get(type)({ target: this });
        }
    }
    const saved = [];
    const ctx = harness(["createSimEntryEl", "escapeHtml"], {
        updateSimEntryStatus() {}, updateSimTxStatus() {},
        document: { createElement: () => new Element() }, channels: new Map(), isChannelListenOnly: () => false,
        channelName: () => "Channel", scheduleAutoSave: reason => saved.push(reason), updateRunningSim: async () => {}, stopSim: async () => {},
    });
    const raw = { kind: "raw", channel: 1, canId: 0x123, isExtended: false, dlc: 1, data: [0], periodMs: 100, running: false, periodicHandle: null };
    const el = ctx.createSimEntryEl("raw", raw);
    for (const [selector, event, value, checked] of [
        [".sim-channel-sel", "change", "2"], [".sim-canid-input", "input", "321"],
        [".sim-ext-cb", "change", "", true], [".sim-dlc-sel", "change", "2"],
        [".sim-period", "input", "250"], [".sim-byte", "input", "AB"],
    ]) {
        const before = saved.length;
        await el.querySelector(selector).fire(event, value, checked);
        assert.equal(saved.length, before + 1, `${selector} must persist without a running transmission`);
    }
    assert.equal(raw.data[0], 0xAB);
    assert.equal(raw.canId, 0x321);
    assert.equal(raw.periodMs, 250);
    const message = { kind: "message", channel: 1, messageId: 1, messageName: "Message", signals: [], periodMs: 100, running: false };
    const msgEl = ctx.createSimEntryEl("msg", message);
    const before = saved.length;
    await msgEl.querySelector(".sim-period").fire("input", "500");
    assert.equal(saved.length, before + 1);
    assert.equal(message.periodMs, 500);
});
