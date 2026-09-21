import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

const source = ts.createSourceFile("main.ts", readFileSync(new URL("../src/main.ts", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true);
const names = ["reflectRecording", "toggleRecording", "refreshRecordingStatus"];
const code = ts.transpileModule(source.statements.filter(n => ts.isFunctionDeclaration(n) && names.includes(n.name?.text)).map(n => n.getText(source)).join("\n"), {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.None },
}).outputText;

function harness(path = null) {
    const indicator = {}, action = {}, calls = [], logs = [];
    let status = { active: false, path: "capture.csv", frames: 12, bytes: 1048576, error: null };
    const context = vm.createContext({
        recordingBusy: false, lastRecordingError: null, appStartTime: 123,
        document: { getElementById: () => indicator, querySelector: () => action },
        dialogSave: async () => path,
        log: (...args) => logs.push(args),
        invoke: async (command, args) => {
            calls.push([command, args]);
            if (command === "start_recording") status = { ...status, active: true };
            if (command === "stop_recording") status = { ...status, active: false };
            return status;
        },
    });
    vm.runInContext(code, context);
    return { context, indicator, action, calls, logs };
}

test("cancelled file selection leaves disk logging off and restores controls", async () => {
    const h = harness();
    await h.context.toggleRecording();
    assert.equal(h.calls.some(([c]) => c === "start_recording"), false);
    assert.equal(h.indicator.hidden, true);
    assert.equal(h.action.disabled, false);
});

test("start shows counters and stop hides the recording indicator", async () => {
    const h = harness("capture.csv");
    await h.context.toggleRecording();
    assert.equal(h.indicator.hidden, false);
    assert.match(h.indicator.textContent, /12 frames.*1.00 MB/);
    assert.equal(h.action.textContent, "Stop logging to disk");
    await h.context.toggleRecording();
    assert.equal(h.indicator.hidden, true);
    assert.equal(h.calls.filter(([c]) => c === "stop_recording").length, 1);
});

test("writer failures are reported once and restored status survives a frontend reload", () => {
    const h = harness();
    const status = { active: true, path: "capture.csv", frames: 5, bytes: 80, error: null };
    h.context.reflectRecording(status);
    assert.equal(h.indicator.hidden, false);
    status.active = false;
    status.error = "Disk full";
    h.context.reflectRecording(status);
    h.context.reflectRecording(status);
    assert.equal(h.indicator.hidden, true);
    assert.equal(h.logs.length, 1);
    assert.deepEqual(h.logs[0], ["error", "Disk full"]);
});
