import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

const source = ts.createSourceFile("main.ts", readFileSync(new URL("../src/main.ts", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true);
const names = ["simEntryStatus", "simFormatError", "canRegisterSim", "updateSimEntryStatus", "updateSimTxStatus", "queueSimOperation",
    "startSim", "stopSim", "openConfiguredChannels", "startApp", "finishStartingCapture", "stopApp", "finishStoppingCapture",
    "onChannelError", "closeFailedChannel", "openChannelByHandle",
    "scheduleChannelRecovery", "attemptChannelRecovery", "recoverChannel", "reconcileSimMessages", "restoreProjectEntries", "simMessageSettings", "defaultSimFormat"];
const declarations = source.statements.filter(n => (ts.isFunctionDeclaration(n) && names.includes(n.name?.text))
    || (ts.isVariableStatement(n) && n.declarationList.declarations.some(d => d.name.getText(source) === "FD_LENGTHS")));
const code = ts.transpileModule(declarations.map(n => n.getText(source)).join("\n"), {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.None },
}).outputText;
function element() {
    const classes = new Set();
    return { textContent: "", disabled: false, title: "", dataset: {}, style: {},
        classList: { add: name => classes.add(name), toggle: (name, on) => on ? classes.add(name) : classes.delete(name), remove: name => classes.delete(name) } };
}
function harness(kind = "raw", invoke = async () => 42) {
    const entry = { kind, channel: 1, canId: 123, messageId: 123, messageName: "Engine", signals: [], data: [0], dlc: 1, periodMs: 100, running: false, periodicHandle: null };
    const ch = { info: { name: "can://localhost/vcan0", backend: "socketcand" },
        config: { listen_only: false }, open: true, available: true };
    const timers = new Map();
    let timerId = 0;
    const controls = Object.fromEntries([".sim-state", ".sim-toggle", ".sim-send-once", ".sim-remove"].map(key => [key, element()]));
    const footer = element(), count = element();
    const row = { querySelector: selector => controls[selector] };
    const ctx = vm.createContext({
        channels: new Map([[1, ch]]), simEntries: new Map([["entry", entry]]), appRunning: true,
        startingCapture: null, stoppingCapture: null, nextSocketcandAttempt: 0,
        document: { querySelector: () => row, getElementById: id => id === "sim-tx-status" ? footer : count },
        invoke, scheduleAutoSave() {}, log() {}, channelName: () => "CAN 1", simSignalValues: () => ({}), simGenerators: () => [],
        viewPaused: false, updatePauseViewBtn() {}, stopBusStatsPoll() {}, renderChannelList() {},
        sigKeyCache: new Map(), pgnMapCache: new Map(),
        channelRecoveryTimers: new Map(), channelRecoveryTasks: new Map(), failedChannelCloses: new Map(),
        pendingSimMessages: [], handleToId: () => "socketcand:can://localhost/vcan0",
        CHANNEL_RECOVERY_INTERVAL_MS: 5000,
        setTimeout: callback => { timers.set(++timerId, callback); return timerId; },
        clearTimeout: id => timers.delete(id),
    });
    vm.runInContext(code, ctx);
    const refresh = () => ctx.updateSimTxStatus();
    const state = () => controls[".sim-state"].textContent;
    return { ctx, entry, ch, controls, footer, count, refresh, state, timers };
}
function deferred() {
    let resolve;
    const promise = new Promise(yes => { resolve = yes; });
    return { promise, resolve };
}
for (const kind of ["raw", "message"]) {
    test(`${kind}: armed entries can be disarmed without implying active TX`, async () => {
        const h = harness(kind, () => assert.fail("Stopped capture must not register a transmission"));
        h.ctx.appRunning = false;
        h.ch.open = false;
        await h.ctx.startSim("entry");
        assert.match(h.state(), /^Armed/);
        assert.equal(h.controls[".sim-toggle"].textContent, "Disarm");
        assert.equal(h.controls[".sim-send-once"].disabled, true);
        assert.equal(h.footer.style.display, "");
        await h.ctx.stopSim("entry");
        assert.equal(h.state(), "Idle");
        assert.equal(h.controls[".sim-toggle"].textContent, "Arm");
    });
    test(`${kind}: pending add/remove and confirmed handles agree with footer`, async () => {
        const add = deferred(), entered = deferred(), remove = deferred(), removing = deferred();
        const h = harness(kind, async command => {
            if (command.startsWith("add_periodic")) { entered.resolve(); return add.promise; }
            removing.resolve(); return remove.promise;
        });
        const starting = h.ctx.startSim("entry");
        assert.match(h.state(), /^Starting.*Registration pending/);
        assert.equal(h.controls[".sim-toggle"].disabled, true);
        assert.equal(h.footer.style.display, "");
        await entered.promise;
        add.resolve(42);
        await starting;
        assert.equal(h.state(), "Transmitting");
        assert.equal(h.controls[".sim-toggle"].textContent, "Stop");
        assert.equal(h.count.textContent, "1 periodic TX");
        assert.equal(h.footer.style.display, "inline-flex");
        const stopping = h.ctx.stopSim("entry");
        await removing.promise;
        assert.match(h.state(), /^Stopping.*registered until removal/);
        assert.equal(h.footer.style.display, "inline-flex");
        remove.resolve();
        await stopping;
        assert.equal(h.state(), "Idle");
        assert.equal(h.footer.style.display, "");
    });
}
test("Stop while registration is delayed remains visibly pending", async () => {
    const add = deferred(), entered = deferred();
    const h = harness("raw", async command => {
        if (command.startsWith("add_periodic")) { entered.resolve(); return add.promise; }
    });
    const starting = h.ctx.startSim("entry");
    await entered.promise;
    const stopping = h.ctx.stopSim("entry");
    assert.match(h.state(), /^Stopping.*pending registration/);
    assert.equal(h.footer.style.display, "");
    add.resolve(42);
    await Promise.all([starting, stopping]);
    assert.equal(h.state(), "Idle");
    assert.equal(h.footer.style.display, "");
});
test("errors expose retry and retain footer count after failed removal", async () => {
    let failAdd = true, failRemove = true;
    const h = harness("raw", async command => {
        if (command.startsWith("add_periodic")) {
            if (failAdd) throw new Error("registration refused");
            return 42;
        }
        if (failRemove) throw new Error("removal refused");
    });
    await h.ctx.startSim("entry");
    assert.match(h.state(), /^Error.*registration refused/);
    assert.equal(h.controls[".sim-toggle"].textContent, "Retry");
    assert.equal(h.footer.style.display, "");
    failAdd = false;
    await h.ctx.startSim("entry");
    await assert.rejects(h.ctx.stopSim("entry"));
    assert.match(h.state(), /^Error.*removal refused.*still registered/);
    assert.equal(h.count.textContent, "1 periodic TX");
    assert.equal(h.footer.style.display, "inline-flex");
    assert.equal(h.controls[".sim-toggle"].textContent, "Stop");
    assert.equal(h.controls[".sim-toggle"].disabled, false);
    failRemove = false;
    await h.ctx.stopSim("entry");
    assert.equal(h.state(), "Idle");
    assert.equal(h.footer.style.display, "");
});
test("unavailable channels explain restrictions and preserve disarm action", () => {
    const h = harness();
    h.ch.available = false;
    h.ch.open = false;
    h.refresh();
    assert.match(h.state(), /^Disconnected.*reconnect/);
    assert.equal(h.controls[".sim-toggle"].disabled, true);
    assert.equal(h.controls[".sim-send-once"].disabled, true);
    h.entry.running = true;
    h.refresh();
    assert.match(h.state(), /armed/);
    assert.equal(h.controls[".sim-toggle"].textContent, "Disarm");
    assert.equal(h.controls[".sim-toggle"].disabled, false);
    h.ch.available = true;
    h.ch.config.listen_only = true;
    h.refresh();
    assert.match(h.state(), /^Listen-only.*sending is disabled/);
    assert.equal(h.controls[".sim-send-once"].disabled, true);
    h.ch.config.listen_only = false;
    h.ch.error = "bus-off";
    h.refresh();
    assert.match(h.state(), /^Disconnected.*bus-off/);
    h.ch.error = null;
    h.ch.open = true;
    h.entry.running = false;
    h.refresh();
    assert.equal(h.state(), "Idle");
    assert.equal(h.controls[".sim-send-once"].disabled, false);
    assert.equal(h.controls[".sim-toggle"].textContent, "Start");
    h.ctx.channels.clear();
    h.refresh();
    assert.match(h.state(), /^Disconnected/);
});

test("a queued Stop clears an error from a failed pending registration", async () => {
    const entered = deferred(), release = deferred();
    const h = harness("raw", async () => {
        entered.resolve();
        await release.promise;
        throw new Error("registration refused");
    });
    const starting = h.ctx.startSim("entry");
    await entered.promise;
    const stopping = h.ctx.stopSim("entry");
    release.resolve();
    await Promise.all([starting, stopping]);
    assert.equal(h.state(), "Idle");
    assert.equal(h.controls[".sim-toggle"].textContent, "Start");
    assert.equal(h.entry.txError, undefined);
});

test("closing capture clears a failed removal error and returns the entry to Armed", async () => {
    const h = harness("raw", async command => {
        if (command.startsWith("add_periodic")) return 42;
        if (command === "remove_periodic") throw new Error("removal refused");
    });
    await h.ctx.startSim("entry");
    await assert.rejects(h.ctx.stopSim("entry"));
    assert.match(h.state(), /^Error/);
    await h.ctx.stopApp();
    assert.match(h.state(), /^Armed/);
    assert.equal(h.entry.periodicHandle, null);
    assert.equal(h.entry.txError, undefined);
    assert.equal(h.footer.style.display, "");
    assert.equal(h.controls[".sim-toggle"].textContent, "Disarm");
});

test("stopping capture disables Send while channel closure is pending", async () => {
    const closing = deferred(), entered = deferred();
    const h = harness("raw", async command => {
        if (command === "close_channel") { entered.resolve(); await closing.promise; }
    });
    h.refresh();
    assert.equal(h.controls[".sim-send-once"].disabled, false);
    const stopping = h.ctx.stopApp();
    await entered.promise;
    assert.equal(h.controls[".sim-send-once"].disabled, true);
    closing.resolve();
    await stopping;
});

test("Stop cancels a Socketcand reconnect and lets capture restart before its response", async () => {
    const open = deferred(), entered = deferred();
    const calls = [];
    const h = harness("raw", async (command, args) => {
        calls.push(command);
        if (command === "open_channel") {
            assert.equal(args.resumeCapture, true);
            assert.equal(args.socketcandAttempt, 1);
            entered.resolve();
            return open.promise;
        }
        assert.equal(command, "close_channel");
        assert.equal(args.socketcandAttempt, 1);
    });
    h.ch.open = false;
    h.entry.running = true;
    h.ctx.scheduleChannelRecovery(1);
    const retry = h.timers.get(1)();
    h.timers.delete(1);
    await entered.promise;
    h.ctx.scheduleChannelRecovery(1);
    assert.equal(h.timers.size, 0);
    h.ctx.scheduleChannelRecovery(2);
    assert.equal(h.timers.size, 1);
    const stopping = h.ctx.stopApp();
    h.ctx.openConfiguredChannels = async () => { calls.push("start"); return false; };
    const starting = h.ctx.startApp();
    assert.equal(h.timers.size, 0);
    await Promise.all([stopping, starting]);
    assert.deepEqual(calls, ["open_channel", "close_channel", "start"]);
    open.resolve(null);
    await retry;
    assert.equal(h.ch.open, false);
    assert.equal(h.entry.periodicHandle, null);
});

test("capture retries when the only Socketcand server is initially offline", async () => {
    const attempts = [];
    const h = harness("raw", async (command, args) => {
        assert.equal(command, "open_channel");
        attempts.push(args.resumeCapture);
        if (attempts.length < 3) throw new Error("Connection refused");
        return null;
    });
    h.ch.open = false;
    Object.assign(h.ctx, {
        appRunning: false, appStartTime: 0, sidebarSnapshot: null,
        ghostChannels: [], refreshHardware: async () => true,
        traceFilterMsgNames: null, signalLastValues: new Map(), signalLastRaw: new Map(),
        signalMinValues: new Map(), signalMaxValues: new Map(), renderDbcTree() {},
        plotPanes: [], restoreProjectEntries: async () => {}, clearTrace() {},
        startScrollLoop() {}, startBusStatsPoll() {},
    });

    await h.ctx.startApp();
    await h.ctx.channelRecoveryTasks.get(1);
    assert.equal(h.ctx.appRunning, true);
    assert.equal(h.ch.open, false);
    assert.match(h.ch.error, /Connection refused/);
    for (const id of [1, 2]) {
        const retry = h.timers.get(id);
        h.timers.delete(id);
        await retry();
    }
    assert.deepEqual(attempts, [false, true, true]);
    assert.equal(h.ch.open, true);
    assert.equal(h.ch.error, null);
    assert.equal(h.timers.size, 0);
});

test("Socketcand startup waits for the capture view, then leaves a slow handshake in the background", async () => {
    const handshake = deferred(), entered = deferred();
    const opened = [];
    let traceReady = false;
    const h = harness("raw", async (command, args) => {
        assert.equal(command, "open_channel");
        opened.push(args.channelHandle);
        if (args.channelHandle === 2) return null;
        assert.equal(traceReady, true);
        assert.equal(h.ctx.appRunning, true);
        entered.resolve();
        return handshake.promise;
    });
    h.ch.open = false;
    const local = { info: { backend: "socketcan", name: "vcan0" }, config: {}, available: true, open: false };
    h.ctx.channels.set(2, local);
    Object.assign(h.ctx, {
        appRunning: false, appStartTime: 0, sidebarSnapshot: null,
        ghostChannels: [], refreshHardware: async startCapture => {
            assert.equal(startCapture, true);
            return true;
        },
        traceFilterMsgNames: null, signalLastValues: new Map(), signalLastRaw: new Map(),
        signalMinValues: new Map(), signalMaxValues: new Map(), renderDbcTree() {},
        plotPanes: [], restoreProjectEntries: async () => {}, clearTrace() { traceReady = true; },
        startScrollLoop() {}, startBusStatsPoll() {},
    });
    await Promise.all([h.ctx.startApp(), entered.promise]);
    assert.deepEqual(opened, [2, 1]);
    assert.equal(local.open, true);
    assert.equal(h.ch.open, false);
    assert.equal(h.count.textContent, "■ Stop");
    handshake.resolve(null);
    await h.ctx.channelRecoveryTasks.get(1);
    assert.equal(h.ch.open, true);
});

test("Socketcand reconnect refreshes DBC definitions before rearming simulated messages", async () => {
    const signal = { name: "Value", factor: 2 };
    const dbc = { messages: { 123: { name: "Updated", dlc: 2, signals: [signal] } } };
    const h = harness("message", async command => {
        if (command === "open_channel") return dbc;
        assert.equal(command, "add_periodic_message");
        assert.equal(h.entry.signals[0].def, signal);
        assert.equal(h.entry.signals[0].value, 12);
        return 42;
    });
    h.ch.open = false;
    h.entry.running = true;
    h.entry.signals = [{ def: { name: "Value", factor: 1 }, value: 12, gen: null }];
    const getElement = h.ctx.document.getElementById;
    h.ctx.document.getElementById = id => id === "sim-entries" ? { querySelector: () => null } : getElement(id);
    h.ctx.renderDbcTree = () => {};
    h.ctx.restoreProjectEntries = async () => {};
    await h.ctx.attemptChannelRecovery(1, true);
    assert.equal(h.entry.periodicHandle, 42);
    assert.equal(h.entry.dlc, 2);
});

test("Socketcand reconnect preserves missing message simulations until their DBC definition returns", async () => {
    const signal = { name: "Value", factor: 2 };
    const generator = { mode: "counter" };
    const missing = { messages: {} };
    const restored = { messages: { 123: { id: 123, name: "Engine", dlc: 2, signals: [signal] } } };
    const opens = [missing, missing, restored];
    let transmissions = 0;
    const h = harness("message", async command => {
        if (command === "open_channel") return opens.shift();
        assert.equal(command, "add_periodic_message");
        transmissions++;
        return 42;
    });
    h.entry.running = true;
    h.entry.signals = [{ def: { name: "Value" }, value: 12, gen: generator }];
    const getElement = h.ctx.document.getElementById;
    h.ctx.document.getElementById = id => id === "sim-entries"
        ? { querySelector: () => ({ remove() {} }), appendChild() {} } : getElement(id);
    Object.assign(h.ctx, {
        pendingPaneSignals: [], plotPanes: [], msgEntryCounter: 0,
        idToHandle: id => id === "socketcand:can://localhost/vcan0" ? 1 : undefined,
        savedDbcMessage: (dbc, id) => dbc?.messages[id],
        renderDbcTree() {}, renderSimEntries() {}, updateSignalHighlights() {},
        createSimEntryEl: () => ({}),
    });

    for (let n = 0; n < 2; n++) {
        h.ch.open = false;
        await h.ctx.attemptChannelRecovery(1, true);
        assert.equal(h.ctx.simEntries.size, 0);
        assert.equal(h.ctx.pendingSimMessages.length, 1);
        const saved = h.ctx.pendingSimMessages[0];
        assert.equal(saved.message_id, 123);
        assert.equal(saved.period_ms, 100);
        assert.equal(saved.running, true);
        assert.equal(saved.signals[0].value, 12);
        assert.deepEqual(saved.signals[0].generator, generator);
        assert.equal(transmissions, 0);
    }

    h.ch.open = false;
    await h.ctx.attemptChannelRecovery(1, true);
    assert.equal(h.ctx.pendingSimMessages.length, 0);
    assert.equal(h.ctx.simEntries.size, 1);
    const entry = [...h.ctx.simEntries.values()][0];
    assert.equal(entry.signals[0].def, signal);
    assert.equal(entry.signals[0].value, 12);
    assert.deepEqual(entry.signals[0].gen, generator);
    assert.equal(entry.periodicHandle, 42);
    assert.equal(transmissions, 1);
});

test("a Socketcand disconnect before the open response leaves the channel disconnected", async () => {
    const open = deferred(), close = deferred(), closing = deferred();
    const h = harness("raw", async (command, args) => {
        if (command === "open_channel") {
            assert.equal(args.resumeCapture, false);
            return open.promise;
        }
        assert.equal(command, "close_channel");
        assert.equal(args.socketcandAttempt, 1);
        closing.resolve();
        return close.promise;
    });
    h.ch.open = false;
    const opening = h.ctx.openChannelByHandle(1);
    const error = h.ctx.onChannelError({ channel_handle: 1, socketcand_attempt: 1, fatal: true, error: "Socketcand disconnected" });
    await closing.promise;
    open.resolve(null);
    await Promise.resolve();
    assert.equal(h.ch.open, false);
    assert.equal(h.ch.error, "Socketcand disconnected");
    close.resolve();
    assert.equal(await opening, false);
    await error;
    assert.equal(h.timers.size, 1);
});

test("Socketcand reconnect retains missing signal settings through repeated reloads and a missing message", async () => {
    const keep = { name: "Keep", min: 0 }, missing = { name: "Temporary", min: 0 };
    const generator = { mode: "counter" };
    const partial = { messages: { 123: { id: 123, name: "Engine", dlc: 2, signals: [keep] } } };
    const full = { messages: { 123: { id: 123, name: "Engine", dlc: 2, signals: [keep, missing] } } };
    const opens = [partial, partial, { messages: {} }, partial, full];
    const h = harness("message", async command => command === "open_channel" ? opens.shift() : 42);
    h.entry.signals = [
        { def: keep, value: 3, gen: null },
        { def: missing, value: 12, gen: generator },
    ];
    const getElement = h.ctx.document.getElementById;
    h.ctx.document.getElementById = id => id === "sim-entries"
        ? { querySelector: () => null, appendChild() {} } : getElement(id);
    Object.assign(h.ctx, {
        pendingPaneSignals: [], plotPanes: [], msgEntryCounter: 0,
        idToHandle: () => 1, savedDbcMessage: (dbc, id) => dbc?.messages[id],
        renderDbcTree() {}, renderSimEntries() {}, updateSignalHighlights() {},
        simDefaultValue: sig => sig.min, createSimEntryEl: () => ({}),
    });
    for (let n = 0; n < 4; n++) {
        h.ch.open = false;
        await h.ctx.attemptChannelRecovery(1, true);
        if (n === 0) h.entry.signals[0].value = 7;
    }
    const partialEntry = [...h.ctx.simEntries.values()][0];
    assert.equal(partialEntry.signals.length, 1, "missing signals must not be sent to the backend");
    h.ch.open = false;
    await h.ctx.attemptChannelRecovery(1, true);
    const entry = [...h.ctx.simEntries.values()][0];
    assert.equal(entry.signals[0].value, 7, "keep edits to surviving signals");
    assert.equal(entry.signals[1].value, 12);
    assert.deepEqual(entry.signals[1].gen, generator);
});

test("a stale Socketcand error cannot close a replacement connection", async () => {
    const calls = [];
    const h = harness("raw", async (command, args) => {
        calls.push([command, args.socketcandAttempt]);
        return null;
    });
    h.ch.open = false;
    h.ctx.nextSocketcandAttempt = 1;
    const opening = h.ctx.openChannelByHandle(1);
    const oldError = { channel_handle: 1, socketcand_attempt: 1, fatal: true, error: "Socketcand disconnected" };
    await h.ctx.onChannelError(oldError);
    assert.equal(await opening, true);
    await h.ctx.onChannelError(oldError);
    assert.equal(h.ch.open, true);
    assert.deepEqual(calls, [["open_channel", 2]]);
    await h.ctx.onChannelError({ ...oldError, socketcand_attempt: 2 });
    assert.equal(h.ch.open, false);
    assert.deepEqual(calls, [["open_channel", 2], ["close_channel", 2]]);
});

test("channel failure replaces a stale removal error with Disconnected", async () => {
    const h = harness("raw", async command => {
        if (command.startsWith("add_periodic")) return 42;
        if (command === "remove_periodic") throw new Error("removal refused");
    });
    await h.ctx.startSim("entry");
    await assert.rejects(h.ctx.stopSim("entry"));
    await h.ctx.onChannelError({ channel_handle: 1, fatal: true, error: "device unplugged" });
    assert.match(h.state(), /^Disconnected.*device unplugged.*armed/);
    assert.equal(h.entry.txError, undefined);
    assert.equal(h.entry.periodicHandle, null);
    assert.equal(h.footer.style.display, "");
    assert.equal(h.controls[".sim-toggle"].textContent, "Disarm");
});

for (const kind of ["raw", "message"]) {
    test(`${kind}: channel failure drains pending registration before closing`, async () => {
        const entered = deferred(), add = deferred();
        const calls = [];
        const h = harness(kind, async command => {
            calls.push(command);
            if (command.startsWith("add_periodic")) {
                entered.resolve();
                return add.promise;
            }
        });
        const starting = h.ctx.startSim("entry");
        await entered.promise;
        const failing = h.ctx.onChannelError({ channel_handle: 1, fatal: true, error: "device unplugged" });
        assert.equal(h.controls[".sim-send-once"].disabled, true);
        await Promise.resolve();
        assert.equal(calls.includes("close_channel"), false);
        add.resolve(42);
        await Promise.all([starting, failing]);
        assert.equal(calls.at(-1), "close_channel");
        assert.equal(h.entry.periodicHandle, null);
        assert.match(h.state(), /^Disconnected/);
        assert.equal(h.footer.style.display, "");
        // Recovery must be able to register the armed entry again.
        h.ch.open = true;
        h.ch.error = null;
        await h.ctx.startSim("entry");
        assert.equal(calls.filter(command => command.startsWith("add_periodic")).length, 2);
        assert.equal(h.state(), "Transmitting");
    });
}
