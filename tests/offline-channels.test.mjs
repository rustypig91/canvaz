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

test("Start with a missing interface does not open any hardware or start capture", async () => {
    let refreshed = false;
    const button = {};
    const ctx = harness(["startApp"], {
        channels: new Map([[1, { available: true }], [2, { available: false }]]),
        ghostChannels: [], appRunning: false,
        refreshHardware: async () => { refreshed = true; return true; },
        renderChannelList: () => {}, document: { getElementById: () => button },
        openChannelByHandle: () => assert.fail("Must not partially start a run"),
        log: () => assert.fail("Missing hardware is not an error"),
    });
    await ctx.startApp();
    assert.equal(refreshed, true);
    assert.equal(ctx.appRunning, false);
    assert.match(button.title, /Connect/);
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
