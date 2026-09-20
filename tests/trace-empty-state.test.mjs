import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

const source = ts.createSourceFile("main.ts", readFileSync(new URL("../src/main.ts", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true);
const fn = source.statements.find(n => ts.isFunctionDeclaration(n) && n.name?.text === "updateTraceEmptyState");
const code = ts.transpileModule(fn.getText(source), { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;

function harness() {
    const elements = new Map();
    const clicks = [];
    for (const id of ["trace-empty", "trace-tbody", "trace-empty-title", "trace-empty-description", "trace-empty-details", "trace-empty-action",
        "btn-add-channel", "btn-app-run", "btn-reload-backends", "btn-clear-filters"]) {
        elements.set(id, { rows: [], hidden: false, textContent: "", click: () => clicks.push(id) });
    }
    const context = vm.createContext({ document: { getElementById: id => elements.get(id) },
        channels: new Map(), ghostChannels: [], traceLocalBuffer: [], appRunning: false, viewPaused: false,
        anyFilterActive: () => context.filtered, filtered: false });
    vm.runInContext(code, context);
    return { context, elements, clicks, title: () => elements.get("trace-empty-title").textContent,
        update: () => context.updateTraceEmptyState(), action: () => elements.get("trace-empty-action").onclick() };
}
const channel = (extra = {}) => ({ config: {}, info: { name: "CAN 1" }, available: true, open: false, ...extra });

test("empty trace progresses from no channels to stopped capture to waiting for traffic", () => {
    const h = harness();
    h.update();
    assert.equal(h.title(), "No channels configured");
    h.action();
    h.context.channels.set(1, channel());
    h.update();
    assert.equal(h.title(), "Capture stopped");
    h.action();
    h.context.appRunning = true;
    h.context.channels.get(1).open = true;
    h.update();
    assert.equal(h.title(), "Waiting for traffic");
    assert.equal(h.elements.get("trace-empty-action").hidden, true);
    assert.deepEqual(h.clicks, ["btn-add-channel", "btn-app-run"]);
});

test("disconnected channels and failed registration preserve diagnostics and reload hardware", () => {
    const h = harness();
    h.context.channels.set(1, channel({ available: false }));
    h.context.channels.set(2, channel({ error: "Driver <failure> & details" }));
    h.context.ghostChannels.push({ config: { name: "Missing backend" }, error: "Library unavailable" });
    h.update();
    assert.match(h.title(), /disconnected/);
    assert.equal(h.elements.get("trace-empty-details").textContent,
        "CAN 1: Interface not found\nCAN 1: Driver <failure> & details\nMissing backend: Library unavailable");
    h.action();
    assert.deepEqual(h.clicks, ["btn-reload-backends"]);
    h.context.channels.set(3, channel({ open: true }));
    h.context.appRunning = true;
    h.update();
    assert.equal(h.title(), "Waiting for traffic");
    assert.match(h.elements.get("trace-empty-details").textContent, /Driver <failure>/);
});

for (const mode of ["overwrite", "append"]) {
    test(`${mode}: hidden frames take priority and visible frames dismiss the empty state`, () => {
        const h = harness();
        h.context.traceLocalBuffer = [{}];
        h.context.filtered = true;
        h.elements.get("trace-tbody").rows = mode === "overwrite" ? [{ dataset: {}, style: { display: "none" } }] : [];
        h.update();
        assert.equal(h.title(), "All frames are hidden by filters");
        h.action();
        assert.deepEqual(h.clicks, ["btn-clear-filters"]);
        h.elements.get("trace-tbody").rows.push({ dataset: {}, style: { display: "" } });
        h.update();
        assert.equal(h.elements.get("trace-empty").hidden, true);
        h.elements.get("trace-tbody").rows = [];
        h.context.traceLocalBuffer = [];
        h.update();
        assert.equal(h.elements.get("trace-empty").hidden, false);
        assert.equal(h.title(), "No channels configured");
    });
}

test("paused incoming traffic does not incorrectly claim filters hide frames", () => {
    const h = harness();
    h.context.channels.set(1, channel({ open: true }));
    Object.assign(h.context, { appRunning: true, viewPaused: true, filtered: true, traceLocalBuffer: [{}] });
    h.update();
    assert.equal(h.title(), "Trace view paused");
});
