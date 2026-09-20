import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

const source = ts.createSourceFile("main.ts", readFileSync(new URL("../src/main.ts", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true);
const names = ["traceFilterCriteria", "anyFilterActive", "clearAllFilters", "setupTraceHeaders"];
const code = ts.transpileModule(source.statements.filter(n => ts.isFunctionDeclaration(n) && names.includes(n.name?.text)).map(n => n.getText(source)).join("\n"), { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;
function harness() {
    const context = vm.createContext({
        traceFilterChannels: null, traceFilterCanIds: null, traceFilterMsgNames: null, traceFilterDir: null,
        traceFilterPgns: null, traceFilterPrios: null, traceFilterSas: null, traceFilterDas: null,
        traceFilterBroadcast: null, traceFilterDlcMin: null, traceFilterDlcMax: null,
        traceFilterCycleMin: null, traceFilterCycleMax: null, traceFilterData: Array(64).fill(null),
        channelName: h => `Channel ${h}`, fmtId: n => n.toString(16), fmtPgn: String, fmtJ1939Addr: n => n.toString(16),
        syncFilteredHeaders() {}, applyTraceFilter() {},
    });
    vm.runInContext(code, context);
    return context;
}

test("summaries include empty selections, unnamed messages, J1939 and each range/data criterion", () => {
    const h = harness();
    Object.assign(h, { traceFilterChannels: new Set([3]), traceFilterCanIds: new Set(), traceFilterMsgNames: new Set(["", "<Engine>"]),
        traceFilterDir: new Set(["rx"]), traceFilterPgns: new Set([-1, 61444]), traceFilterPrios: new Set([0]),
        traceFilterSas: new Set([128]), traceFilterDas: new Set([255]), traceFilterBroadcast: false,
        traceFilterDlcMin: 0, traceFilterDlcMax: 64, traceFilterCycleMin: 0, traceFilterCycleMax: 100 });
    h.traceFilterData[0] = 0;
    h.traceFilterData[63] = 255;
    const criteria = h.traceFilterCriteria();
    assert.equal(criteria.length, 15);
    const texts = Object.fromEntries(criteria.map(c => [c.key, c.text]));
    assert.equal(texts.canId, "CAN ID: None (all frames hidden)");
    assert.equal(texts.msg, "Message: (no message), <Engine>");
    assert.equal(texts.pgn, "PGN: (non-J1939), 61444");
    assert.equal(texts.broadcast, "PGN type: Destination-specific");
    assert.equal(texts["data-63"], "Data byte 63: 0xFF");
    for (const criterion of criteria) {
        const before = h.traceFilterCriteria().length;
        criterion.clear();
        assert.equal(h.traceFilterCriteria().length, before - 1, criterion.key);
    }
    assert.equal(h.anyFilterActive(), false);
});

test("clear all removes every criterion and preserves byte count", () => {
    const h = harness();
    h.traceFilterDir = new Set(); h.traceFilterBroadcast = true;
    h.traceFilterDlcMax = 8; h.traceFilterData[63] = 1;
    h.clearAllFilters();
    assert.equal(h.traceFilterCriteria().length, 0);
    assert.equal(h.traceFilterData.length, 64);
    assert.equal(h.anyFilterActive(), false);
});

test("visible filter activation and context menu share behavior without sorting or dragging", () => {
    const h = harness();
    const element = () => ({ listeners: {}, classList: { add() {} }, dataset: {},
        addEventListener(name, fn) { this.listeners[name] = fn; },
        appendChild(child) { this.child = child; }, setAttribute() {},
        getBoundingClientRect() { return { left: 10, bottom: 30 }; }, focus() { this.focused = true; } });
    const th = element(); th.dataset.col = "dir";
    const input = element(); const menu = element();
    menu.querySelector = () => input; menu.remove = () => { menu.removed = true; };
    Object.assign(h, { traceHeaderEls: [th], TRACE_COL_DEFS: [{ key: "dir", label: "Dir" }],
        document: { querySelectorAll: () => [], createElement: element },
        traceAbsorberCol: () => "dir", visibleTraceCols: () => ["dir"], updateSortIndicators() {},
        showFilterMenu(x, y) { h.coords = [x, y]; h.ctxMenu = menu; },
    });
    h.setupTraceHeaders();
    let stopped = false;
    th.child.listeners.click({ stopPropagation() { stopped = true; } });
    assert.equal(stopped, true);
    assert.deepEqual(h.coords, [10, 30]);
    assert.equal(input.focused, true);
    menu.listeners.keydown({ key: "Escape", stopPropagation() {} });
    assert.equal(menu.removed, true);
    assert.equal(th.child.focused, true);
    th.listeners.contextmenu({ clientX: 40, clientY: 50, preventDefault() {} });
    assert.deepEqual(h.coords, [40, 50]);
    stopped = false;
    th.child.listeners.mousedown({ stopPropagation() { stopped = true; } });
    assert.equal(stopped, true);
});
