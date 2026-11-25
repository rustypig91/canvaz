import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

const source = ts.createSourceFile("main.ts", readFileSync(new URL("../src/main.ts", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true);
function harness(names, globals = {}) {
    names = [...new Set([...names, "defaultSimFormat", "simFormatControls"])];
    const constants = source.statements.filter(n => ts.isVariableStatement(n)
        && n.declarationList.declarations.some(d => d.name.getText(source) === "FD_LENGTHS"));
    const functions = source.statements.filter(n => ts.isFunctionDeclaration(n) && names.includes(n.name?.text));
    assert.equal(functions.length, names.length);
    const code = ts.transpileModule([...constants, ...functions].map(n => n.getText(source)).join("\n").replaceAll("import.meta.env.DEV", "false"), {
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
        startingCapture: null, stoppingCapture: null,
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
    assert.match(el.innerHTML, /<option value="0"/);
    for (const [selector, event, value, checked] of [
        [".sim-channel-sel", "change", "2"], [".sim-canid-input", "change", "321"],
        [".sim-ext-cb", "change", "", true], [".sim-dlc-sel", "change", "0"],
        [".sim-period", "input", "250"], [".sim-byte", "input", "AB"],
    ]) {
        const before = saved.length;
        await el.querySelector(selector).fire(event, value, checked);
        assert.equal(saved.length, before + 1, `${selector} must persist without a running transmission`);
    }
    assert.equal(raw.data[0], 0xAB);
    assert.equal(raw.canId, 0x321);
    assert.equal(raw.periodMs, 250);
    assert.equal(raw.dlc, 0);
    assert.equal(el.querySelector(".sim-byte").disabled, true);
    assert.match(ctx.createSimEntryEl("restored", raw).innerHTML, /<option value="0" selected>/);
    const message = { kind: "message", channel: 1, messageId: 1, messageName: "Message", signals: [], periodMs: 100, running: false };
    const msgEl = ctx.createSimEntryEl("msg", message);
    const before = saved.length;
    await msgEl.querySelector(".sim-period").fire("input", "500");
    assert.equal(saved.length, before + 1);
    assert.equal(message.periodMs, 500);
});

test("opening another project resets backend registrations and cached traffic before registering channels", async () => {
    const calls = [];
    const oldChannels = new Map([[7, { info: { name: "old" } }]]);
    const caches = Object.fromEntries(["signalLastValues", "signalLastRaw", "signalMinValues", "signalMaxValues", "sigKeyCache", "pgnMapCache"].map(k => [k, new Map([["old", 1]])]));
    const ctx = harness(["applyProject"], {
        ...caches, appRunning: true, channels: oldChannels, ghostChannels: [], plotPanes: [],
        simEntries: new Map(), pendingPaneSignals: [], pendingSimMessages: [], DEFAULT_WINDOW_SEC: 60,
        stopApp: async () => calls.push("stop"),
        invoke: async command => {
            assert.equal(command, "reset_backend");
            assert.equal(oldChannels.size, 1);
            calls.push("reset");
        },
        clearTrace: () => calls.push("clear trace"),
        registerChannel: async config => {
            assert.equal(oldChannels.size, 0);
            for (const cache of Object.values(caches)) assert.equal(cache.size, 0);
            calls.push(`register ${config.name}`);
            return { handle: 8 };
        },
        document: { getElementById: () => ({ innerHTML: "" }) },
        refreshChannelList() {}, renderDbcTree() {}, rebuildTraceColumns() {}, setWindowSize() {},
        restoreProjectEntries: async () => {},
    });
    await ctx.applyProject({ channels: [{ name: "new" }], plot_panes: [] });
    assert.deepEqual(calls, ["stop", "reset", "clear trace", "register new"]);
});

test("restored short raw payloads can grow from zero to eight bytes and transmit the chosen DLC", async () => {
    for (const data of [[], [0xAB]]) {
        const caches = Object.fromEntries(["signalLastValues", "signalLastRaw", "signalMinValues", "signalMaxValues", "sigKeyCache", "pgnMapCache"].map(k => [k, new Map()]));
        const sent = [];
        const ctx = harness(["applyProject", "sendSimOnce", "startSim", "queueSimOperation", "updateRunningSim"], {
            ...caches, appRunning: false, startingCapture: null, stoppingCapture: null,
            channels: new Map(), ghostChannels: [], plotPanes: [], rawEntryCounter: 0,
            simEntries: new Map(), pendingPaneSignals: [], pendingSimMessages: [], DEFAULT_WINDOW_SEC: 60,
            invoke: async (command, args) => {
                if (command !== "reset_backend") sent.push([command, Array.from(args.cmd.data)]);
                return 42;
            },
            clearTrace() {}, idToHandle: () => 1,
            document: { getElementById: () => ({ innerHTML: "", appendChild() {} }) }, createSimEntryEl() {},
            refreshChannelList() {}, renderDbcTree() {}, rebuildTraceColumns() {}, setWindowSize() {},
            restoreProjectEntries: async () => {}, updateSimTxStatus() {}, scheduleAutoSave() {},
        });
        await ctx.applyProject({ channels: [], plot_panes: [], simulate_raw_frames: [{
            channel: "socketcand:can://localhost/vcan0", can_id: 0x123, is_extended: false,
            dlc: data.length, data, period_ms: 100,
        }] });
        ctx.appRunning = true;
        ctx.channels.set(1, { open: true, config: {} });
        const [key, entry] = [...ctx.simEntries][0];
        assert.equal(entry.data.length, 8, "restore all eight editable byte slots");
        await ctx.sendSimOnce(key);
        assert.deepEqual(sent.pop(), ["send_frame", data]);
        entry.dlc = 8;
        const expected = Array.from({ length: 8 }, (_, i) => data[i] ?? 0);
        await ctx.sendSimOnce(key);
        await ctx.startSim(key);
        await ctx.updateRunningSim(key);
        assert.deepEqual(sent, [["send_frame", expected], ["add_periodic_frame", expected], ["update_periodic_frame", expected]]);
    }
});

test("FD raw editor offers legal lengths and delivers byte 64 to Send and Simulate", async () => {
    class Element {
        dataset = {};
        addEventListener() {}
        querySelector() { return new Element(); }
        querySelectorAll() { return []; }
    }
    const sent = [];
    const ctx = harness(["createSimEntryEl", "escapeHtml", "sendSimOnce", "startSim", "queueSimOperation", "updateRunningSim"], {
        channels: new Map([[1, { open: true, info: { backend: "socketcan", name: "vcan0" }, config: { mode: "fd" } }]]),
        simEntries: new Map(), appRunning: true,
        document: { createElement: () => new Element() }, updateSimEntryStatus() {},
        updateSimTxStatus() {}, scheduleAutoSave() {},
        invoke: async (command, { cmd }) => { sent.push([command, cmd]); return 42; },
    });
    const data = Array.from({ length: 64 }, (_, i) => i);
    const entry = {
        kind: "raw", channel: 1, canId: 0x123, isExtended: true, format: "fd", brs: true, dlc: 64,
        data, periodMs: 100, running: false, periodicHandle: null,
    };
    ctx.simEntries.set("fd", entry);
    const editor = ctx.createSimEntryEl("fd", entry).innerHTML;
    assert.match(editor, /<option value="64" selected>/);
    assert.doesNotMatch(editor, /<option value="63"/);
    assert.match(editor, /Data byte 64 in hexadecimal/);
    await ctx.sendSimOnce("fd");
    await ctx.startSim("fd");
    await ctx.updateRunningSim("fd");
    assert.deepEqual(sent.map(([command]) => command), ["send_frame", "add_periodic_frame", "update_periodic_frame"]);
    for (const [, command] of sent) {
        assert.equal(command.format, "fd");
        assert.equal(command.brs, true);
        assert.equal(command.is_extended, true);
        assert.deepEqual(Array.from(command.data), data);
    }
});

test("rejected FD simulator edits leave the running registration active", async () => {
    const calls = [];
    const { ctx, entry } = simHarness(async command => {
        calls.push(command);
        if (command === "add_periodic_frame") return 42;
        throw new Error("Invalid CAN FD payload length");
    });
    entry.format = "fd";
    await ctx.startSim("raw");
    await ctx.updateRunningSim("raw");
    assert.deepEqual(calls, ["add_periodic_frame", "update_periodic_frame"]);
    assert.equal(entry.running, true);
    assert.equal(entry.periodicHandle, 42);
    assert.match(entry.txError, /Invalid CAN FD payload length/);
});

test("Trace rows keep an explicit DBC mismatch through expansion and column rebuilds", () => {
    const message = { id: 0, signals: [] };
    const ctx = harness(["buildTraceRow", "entryFromRow", "expandTraceRow", "traceRowDbcMessageId", "dbcMessageFor", "dbcMessageId"], {
        channels: new Map([[1, { dbc: { messages: { 0: message } }, config: {} }]]),
        traceEntryRows: new WeakMap(), traceRowSignals: new WeakMap(),
        traceRowVisible: () => true, visibleTraceCols: () => [],
        document: { createElement: tag => {
            assert.equal(tag, "tr", "an incompatible frame must not open a signal expansion");
            return { dataset: {}, classList: { add() {}, contains: name => name === "dbc-match" } };
        } },
    });
    const entry = {
        channelHandle: 1, canId: 0, isExtended: false, timestampMs: 100, format: "fd",
        direction: "rx", messageName: null, dlc: 0, data: [], signals: [],
    };
    const unmatched = ctx.buildTraceRow({ ...entry, dbcMessageId: null });
    assert.equal(ctx.entryFromRow(unmatched).dbcMessageId, null);
    ctx.expandTraceRow(unmatched);
    const decoded = ctx.buildTraceRow({ ...entry, dbcMessageId: 0 });
    assert.equal(ctx.entryFromRow(decoded).dbcMessageId, 0);
    const legacy = ctx.buildTraceRow(entry);
    assert.equal(ctx.entryFromRow(legacy).dbcMessageId, undefined);
});

test("retained CAN FD and Classical frames with the same ID track independent cycle times", async () => {
    const frames = [
        { format: "fd", brs: true, esi: true, timestamp_ms: 100, dlc: 8, data: Array(8).fill(0) },
        { format: "classical", timestamp_ms: 103, dlc: 8, data: Array(8).fill(0) },
        { format: "fd", brs: true, timestamp_ms: 110, dlc: 64, data: Array(64).fill(0xAB) },
        { format: "classical", timestamp_ms: 123, dlc: 8, data: Array(8).fill(0) },
    ].map(frame => ({ ...frame, can_id: 0x123, is_extended: true, channel_handle: 1, direction: "rx" }));
    const ctx = harness(["loadTraceFrames", "traceKey"], {
        invoke: async () => frames, traceMaxRows: 100, traceLoadGeneration: 0, traceLocalBuffer: [],
        traceSeenChannels: new Set(), traceSeenCanIds: new Set(), traceSeenMsgNames: new Set(),
        traceSeenNoJ1939: false, interleaveFrameSignals: () => [],
    });
    assert.equal(await ctx.loadTraceFrames(), true);
    const [classic, longFd, firstClassic, shortFd] = ctx.traceLocalBuffer;
    assert.equal(classic.cycleTimeMs, 20);
    assert.equal(longFd.cycleTimeMs, 10);
    assert.equal(longFd.brs, true);
    assert.equal(longFd.data[63], 0xAB);
    assert.equal(firstClassic.cycleTimeMs, null);
    assert.equal(shortFd.cycleTimeMs, null);
    assert.equal(shortFd.esi, true);
});

test("clearing trace invalidates an in-flight fetch from the previous project", async () => {
    const pending = deferred();
    const caches = Object.fromEntries([
        "traceRowEls", "tracePendingOverwrite", "traceLastTs", "traceSeenChannels",
        "traceSeenCanIds", "traceSeenMsgNames", "traceSeenPgns", "traceSeenPrios",
        "traceSeenSas", "traceSeenDas",
    ].map(key => [key, new Map([["old", 1]])]));
    const ctx = harness(["loadTraceFrames", "clearTrace"], {
        ...caches, traceLoadGeneration: 0, traceMaxRows: 100,
        traceLocalBuffer: ["old"], traceSeenNoMsg: true, traceSeenNoJ1939: true,
        invoke: () => pending.promise,
        destroyAllTracePlots() {}, updateTraceEmptyState() {},
        document: { getElementById: () => ({ innerHTML: "old" }) },
    });
    const loading = ctx.loadTraceFrames();
    ctx.clearTrace();
    // A stale frame must never reach mapping (or repopulate filter caches).
    pending.resolve([{ channel_handle: 7, can_id: 123 }]);
    assert.equal(await loading, false);
    assert.equal(ctx.traceLocalBuffer.length, 0);
    for (const cache of Object.values(caches)) assert.equal(cache.size, 0);
});

test("an older trace response cannot overwrite a newer load", async () => {
    const first = deferred(), second = deferred();
    const responses = [first.promise, second.promise];
    const ctx = harness(["loadTraceFrames"], {
        traceLoadGeneration: 0, traceMaxRows: 100, traceLocalBuffer: ["old"],
        invoke: () => responses.shift(),
    });
    const older = ctx.loadTraceFrames();
    const newer = ctx.loadTraceFrames();
    second.resolve([]);
    assert.equal(await newer, true);
    first.resolve([{ channel_handle: 7, can_id: 123 }]);
    assert.equal(await older, false);
    assert.equal(ctx.traceLocalBuffer.length, 0);
});

test("a superseded resume fetch leaves the current trace view intact", async () => {
    const pending = deferred();
    const tbody = { innerHTML: "new project rows" };
    const ctx = harness(["resumeFromPause"], {
        sidebarSnapshot: {}, refreshSidebarValues() {},
        document: { getElementById: () => tbody },
        traceMode: "append", loadTraceFrames: () => pending.promise,
        destroyAllTracePlots() { assert.fail("stale resume must not destroy current plots"); },
        appStartTime: Date.now(), plotPanes: [], startScrollLoop() {},
    });
    ctx.resumeFromPause();
    pending.resolve(false);
    await new Promise(resolve => setImmediate(resolve));
    assert.equal(tbody.innerHTML, "new project rows");
});
