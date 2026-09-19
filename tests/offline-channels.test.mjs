import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

// Exercise the production functions in the single-file frontend with mocked
// Tauri IPC. No CAN driver or physical interface is needed for these scenarios.
const source = ts.createSourceFile("main.ts", readFileSync(new URL("../src/main.ts", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true);
function harness(names, globals) {
    const functions = source.statements.filter(n => ts.isFunctionDeclaration(n) && names.includes(n.name?.text));
    assert.equal(functions.length, names.length);
    const code = ts.transpileModule(functions.map(n => n.getText(source)).join("\n"), {
        compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.None },
    }).outputText;
    const context = vm.createContext(globals);
    vm.runInContext(code, context);
    return context;
}
const config = () => ({ name: "USB CAN", backend: "kvaser", dbc_path: "vehicle.dbc", bitrate: 500000, protocol: null, listen_only: false });

test("missing hardware still registers a normal channel and loads its DBC", async () => {
    const dbc = { messages: { 123: { id: 123, name: "Engine", signals: [] } } };
    const calls = [];
    const ctx = harness(["registerChannel", "loadChannelDbc"], {
        channels: new Map(), sigKeyCache: new Map(), pgnMapCache: new Map(),
        log: () => assert.fail("Offline registration must not log an error"),
        invoke: async (command) => {
            calls.push(command);
            if (command === "create_channel") return { handle: 7, backend: "kvaser", available: false };
            if (command === "parse_dbc") return dbc;
            assert.fail(command);
        },
    });
    const result = await ctx.registerChannel(config());
    assert.equal(result.handle, 7);
    assert.equal(ctx.channels.get(7).dbc, dbc);
    assert.equal(ctx.channels.get(7).available, false);
    assert.equal(ctx.channels.get(7).open, false);
    assert.deepEqual(calls, ["create_channel", "parse_dbc"]);
});

test("Start reports missing interfaces and continues past failures to open other channels", async () => {
    const attempted = [], errors = [];
    const ctx = harness(["openConfiguredChannels"], {
        channels: new Map([[1, { available: false, config: {}, info: { name: "Missing" } }],
            [2, { available: true }], [3, { available: true }]]),
        ghostChannels: [], refreshHardware: async () => true, renderChannelList: () => {},
        openChannelByHandle: async handle => { attempted.push(handle); return handle === 3; },
        log: (level, message) => errors.push({ level, message }),
    });
    assert.equal(await ctx.openConfiguredChannels(true), true);
    assert.deepEqual(attempted, [2, 3]);
    assert.equal(errors.length, 1);
    assert.equal(errors[0].level, "error");
    assert.match(errors[0].message, /Missing.*interface not found/);
});

test("all missing channels keep capture stopped, with errors only on explicit Start", async () => {
    const errors = [];
    const ctx = harness(["openConfiguredChannels", "startApp"], {
        channels: new Map([[1, { available: false, config: {}, info: { name: "Missing" } }]]),
        ghostChannels: [], appRunning: false,
        refreshHardware: async () => true, renderChannelList: () => {},
        openChannelByHandle: () => assert.fail("Missing hardware must not be opened"),
        log: (level, message) => errors.push({ level, message }),
    });
    await ctx.startApp(false);
    assert.equal(errors.length, 0);
    await ctx.startApp();
    assert.equal(errors.length, 1);
    assert.equal(ctx.appRunning, false);
});

test("hardware refresh preserves offline channel data and reconnects the same handle", async () => {
    const ch = { config: config(), info: { backend: "kvaser", name: "USB CAN" }, dbc: { messages: {} }, open: false, available: false };
    let available = false;
    const ctx = harness(["refreshHardware"], {
        channels: new Map([[7, ch]]), ghostChannels: [], renderChannelList: () => {},
        invoke: async () => [{ old_handle: 7, new_handle: 7, backend: "kvaser", available }],
        log: () => assert.fail("Hardware refresh must not report missing interfaces as errors"),
    });
    assert.equal(await ctx.refreshHardware(), true);
    assert.equal(ctx.channels.get(7).available, false);
    available = true;
    assert.equal(await ctx.refreshHardware(), true);
    assert.equal(ctx.channels.get(7).available, true);
    assert.equal(ctx.channels.get(7).dbc, ch.dbc);
    assert.equal(ctx.channels.get(7).config, ch.config);
    assert.equal(ctx.ghostChannels.length, 0);
});

test("saved plots and simulation settings restore while disconnected without transmitting", async () => {
    const signal = { name: "RPM", message_id: 123 };
    const message = { id: 123, name: "Engine", dlc: 8, signals: [signal] };
    const added = [];
    const ctx = harness(["restoreProjectEntries"], {
        channels: new Map([[7, { available: false, open: false, dbc: { messages: { 123: message } } }]]),
        pendingPaneSignals: [[{ channel: "kvaser:USB CAN", message_id: 123, signal_name: "RPM" }]],
        pendingSimMessages: [{ channel: "kvaser:USB CAN", message_id: 123, period_ms: 100, running: true, signals: [{ name: "RPM", value: 900 }] }],
        plotPanes: [{ id: 1 }], simEntries: new Map(), msgEntryCounter: 0,
        idToHandle: () => 7,
        addSignalToPane: async (pane, handle, sig) => added.push({ pane, handle, sig }),
        document: { getElementById: () => ({ appendChild: () => {} }) },
        createSimEntryEl: () => ({}), renderSimEntries: () => {}, updateSignalHighlights: () => {},
        invoke: () => assert.fail("Restoration must not transmit or open hardware"),
    });
    await ctx.restoreProjectEntries();
    assert.equal(added.length, 1);
    assert.equal(added[0].handle, 7);
    assert.equal(added[0].sig, signal);
    const entry = [...ctx.simEntries.values()][0];
    assert.equal(entry.signals[0].value, 900);
    assert.equal(entry.running, true);
    assert.equal(entry.periodicHandle, null);
    assert.equal(ctx.pendingPaneSignals.length, 0);
    assert.equal(ctx.pendingSimMessages.length, 0);
});

test("Start preserves duplicate offline channels, dependent entries and saved IDs until reconnection", async () => {
    for (const reverse of [false, true]) {
        const original = { config: config(), info: { backend: "kvaser", name: "USB CAN" }, dbc: { messages: {} }, open: false };
        const message = { id: 123, name: "Engine", dlc: 8, signals: [] };
        const duplicate = { config: { ...config(), backend: "pcan", bitrate: 250000 }, info: { backend: "pcan", name: "USB CAN" }, dbc: { messages: { 123: message } }, open: false };
        const entries = [[7, original], [8, duplicate]];
        const simulated = { kind: "message", channel: 8, messageId: 123, signals: [], running: false };
        const raw = { kind: "raw", channel: 8, running: false };
        const plotted = { channel: 8, timestamps: [], data: [] };
        const pane = { series: new Map([["rpm", plotted]]), chart: { options: { scales: { x: {} } }, update: () => {} } };
        let reconnected = false;
        const opened = [];
        const element = { value: "", querySelector: () => null, classList: { add: () => {} } };
        const ctx = harness(["refreshHardware", "registerChannel", "handleToId", "idToHandle", "openConfiguredChannels", "startApp"], {
            channels: new Map(reverse ? entries.reverse() : entries), ghostChannels: [], renderChannelList: () => {},
            simEntries: new Map([["message", simulated], ["raw", raw]]), plotPanes: [pane],
            document: { getElementById: () => element },
            appRunning: false, appStartTime: 0, traceFilterMsgNames: null,
            signalLastValues: new Map(), signalLastRaw: new Map(), signalMinValues: new Map(), signalMaxValues: new Map(),
            viewPaused: false, sidebarSnapshot: null, windowSizeSec: 10,
            renderDbcTree: () => {}, updatePauseViewBtn: () => {}, setPaneCursors: () => {}, clearPaneZoom: () => {},
            restoreProjectEntries: async () => {}, clearTrace: () => {}, startScrollLoop: () => {}, startBusStatsPoll: () => {}, log: () => {},
            openChannelByHandle: async handle => { opened.push(handle); return true; },
            invoke: async (command) => {
                if (command === "reload_backends") return [
                    { old_handle: 7, new_handle: 7, backend: "kvaser", available: true },
                    { old_handle: 8, new_handle: reconnected ? 8 : 7, backend: reconnected ? "pcan" : "kvaser", available: true },
                ];
                assert.fail(command);
            },
        });
        await ctx.startApp();
        assert.deepEqual(opened, [7]);
        assert.equal(ctx.channels.size, 2);
        assert.equal(ctx.channels.get(7).config, original.config);
        assert.equal(ctx.channels.get(7).dbc, original.dbc);
        assert.equal(ctx.channels.get(8).config, duplicate.config);
        assert.equal(ctx.channels.get(8).dbc, duplicate.dbc);
        assert.equal(ctx.channels.get(8).available, false);
        assert.equal(ctx.ghostChannels.length, 0);
        assert.equal(duplicate.config.backend, "pcan");
        assert.equal(ctx.simEntries.get("message"), simulated);
        for (const entry of [simulated, raw, plotted]) {
            assert.equal(ctx.handleToId(entry.channel), "pcan:USB CAN");
            assert.equal(ctx.idToHandle(ctx.handleToId(entry.channel)), 8);
        }
        reconnected = true;
        assert.equal(await ctx.refreshHardware(), true);
        assert.equal(ctx.channels.get(8).available, true);
        assert.equal(ctx.channels.get(8).dbc, duplicate.dbc);
        assert.equal(ctx.idToHandle("pcan:USB CAN"), 8);
    }
});

test("one-shot simulation sends only on an open channel and preserves offline settings", async () => {
    for (const kind of ["message", "raw"]) {
        const entry = { kind, channel: 7, messageId: 123, canId: 123, data: [1, 2, 3], dlc: 2, isExtended: true, running: true, periodicHandle: null };
        const before = structuredClone(entry);
        const calls = [], warnings = [];
        const ctx = harness(["sendSimOnce"], {
            simEntries: new Map([["entry", entry]]), channels: new Map([[7, { open: false }]]),
            invoke: async (command, args) => calls.push({ command, args }),
            log: (level, message) => warnings.push({ level, message }),
            simSignalValues: () => ({ RPM: 900 }), simGenerators: () => [],
        });
        await ctx.sendSimOnce("entry");
        assert.equal(calls.length, 0);
        assert.equal(warnings[0].level, "warn");
        assert.deepEqual(entry, before);
        ctx.channels.get(7).open = true;
        await ctx.sendSimOnce("entry");
        assert.equal(calls.length, 1);
        assert.equal(calls[0].command, kind === "message" ? "send_message" : "send_frame");
        assert.equal(calls[0].args.cmd.channel_handle, 7);
        if (kind === "raw") assert.deepEqual(calls[0].args.cmd.data, [1, 2]);
    }
});
