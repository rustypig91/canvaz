import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

const source = ts.createSourceFile("main.ts", readFileSync(new URL("../src/main.ts", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true);
const names = ["updatePauseViewBtn", "updateTraceModeControls", "setTraceMode"];
const code = ts.transpileModule(source.statements.filter(n => ts.isFunctionDeclaration(n) && names.includes(n.name?.text)).map(n => n.getText(source)).join("\n"), {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.None },
}).outputText;

function button() {
    const classes = new Set();
    const attributes = new Map();
    return {
        textContent: "", title: "", disabled: false, attributes,
        classList: { toggle: (name, force) => force ? classes.add(name) : classes.delete(name), contains: name => classes.has(name) },
        setAttribute: (name, value) => attributes.set(name, value),
    };
}

function harness() {
    const buttons = new Map([["btn-pause-view", button()], ["btn-trace-latest", button()], ["btn-trace-all", button()]]);
    const context = vm.createContext({
        document: { getElementById: id => buttons.get(id) }, traceMode: "overwrite", viewPaused: false, appRunning: true,
        clearTrace() { context.clears = (context.clears ?? 0) + 1; }, updateTraceEmptyState() {},
    });
    vm.runInContext(code, context);
    return { context, buttons };
}

test("pause control names the view and explains background capture", () => {
    const { context, buttons } = harness();
    context.updatePauseViewBtn();
    assert.equal(buttons.get("btn-pause-view").textContent, "Pause view");
    assert.match(buttons.get("btn-pause-view").title, /Capture and configured transmissions continue/);
    context.viewPaused = true;
    context.updatePauseViewBtn();
    assert.equal(buttons.get("btn-pause-view").textContent, "Resume view");
    assert.match(buttons.get("btn-pause-view").title, /have continued/);
});

test("trace mode choices expose matching active and pressed states", () => {
    const { context, buttons } = harness();
    context.updateTraceModeControls();
    assert.equal(buttons.get("btn-trace-latest").attributes.get("aria-pressed"), "true");
    assert.equal(buttons.get("btn-trace-all").attributes.get("aria-pressed"), "false");
    context.setTraceMode("append");
    assert.equal(context.clears, 1);
    assert.equal(buttons.get("btn-trace-latest").attributes.get("aria-pressed"), "false");
    assert.equal(buttons.get("btn-trace-all").attributes.get("aria-pressed"), "true");
});
