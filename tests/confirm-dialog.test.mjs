import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

const source = ts.createSourceFile("main.ts", readFileSync(new URL("../src/main.ts", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true);
const names = ["showConfirm", "confirmAndStop", "newProject", "openProject"];
const code = ts.transpileModule(source.statements.filter(n => ts.isFunctionDeclaration(n) && names.includes(n.name?.text)).map(n => n.getText(source)).join("\n"), {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.None },
}).outputText;

function harness() {
    const document = { activeElement: null, getElementById: id => elements.get(id) };
    class Element extends EventTarget {
        textContent = "";
        focus() { document.activeElement = this; }
        showModal() { this.open = true; }
        close() { this.open = false; }
    }
    const elements = new Map(["dialog-confirm", "dialog-confirm-msg", "btn-confirm-ok", "btn-confirm-cancel"].map(id => [id, new Element()]));
    let dialogOpenCalls = 0;
    const context = vm.createContext({
        document, appRunning: true, projectDirty: true, projectRevision: 7,
        projectPath: "original.canvaz", restoringProject: false,
        stopApp: async () => { context.appRunning = false; },
        dialogOpen: async () => { dialogOpenCalls++; return "another.canvaz"; },
        invoke: async command => { assert.equal(command, "load_project"); return {}; },
        log: () => assert.fail("Unexpected project error"),
    });
    vm.runInContext(code, context);
    return { context, elements, document, getDialogOpenCalls: () => dialogOpenCalls };
}

test("confirmation uses each caller's label and initially focuses Cancel", async () => {
    const { context, elements, document } = harness();
    for (const label of ["Discard changes", "Stop & remove channel"]) {
        const result = context.showConfirm("Specific consequence?", label);
        assert.equal(elements.get("btn-confirm-ok").textContent, label);
        assert.equal(elements.get("dialog-confirm-msg").textContent, "Specific consequence?");
        assert.equal(document.activeElement, elements.get("btn-confirm-cancel"));
        elements.get("btn-confirm-ok").dispatchEvent(new Event("click"));
        assert.equal(await result, true);
        assert.equal(elements.get("dialog-confirm").open, false);
    }
});

for (const [target, eventType] of [["btn-confirm-cancel", "click"], ["dialog-confirm", "cancel"]]) {
    test(`${eventType} preserves capture and unsaved projects`, async () => {
        for (const operation of ["confirmAndStop", "newProject", "openProject"]) {
            const { context, elements, getDialogOpenCalls } = harness();
            const result = context[operation]("Stop live capture to add a channel?", "Stop & add channel");
            await new Promise(resolve => setImmediate(resolve));
            assert.equal(elements.get("dialog-confirm").open, true);
            if (operation !== "confirmAndStop") assert.equal(elements.get("btn-confirm-ok").textContent, "Discard changes");
            const event = new Event(eventType, { cancelable: true });
            elements.get(target).dispatchEvent(event);
            await result;
            assert.equal(event.defaultPrevented, true);
            assert.equal(elements.get("dialog-confirm").open, false);
            assert.equal(context.appRunning, true);
            assert.equal(context.projectDirty, true);
            assert.equal(context.projectRevision, 7);
            assert.equal(context.projectPath, "original.canvaz");
            assert.equal(context.restoringProject, false);
            if (operation === "openProject") assert.equal(getDialogOpenCalls(), 0);
        }
    });
}

test("capture stops only after confirmation; stopped capture needs no prompt", async () => {
    const { context, elements } = harness();
    const result = context.confirmAndStop("Stop live capture to reload CAN backends?", "Stop & reload backends");
    assert.equal(context.appRunning, true);
    assert.equal(elements.get("btn-confirm-ok").textContent, "Stop & reload backends");
    elements.get("btn-confirm-ok").dispatchEvent(new Event("click"));
    assert.equal(await result, true);
    assert.equal(context.appRunning, false);
    assert.equal(await context.confirmAndStop("Unused", "Unused"), true);
    assert.equal(elements.get("dialog-confirm").open, false);
});
