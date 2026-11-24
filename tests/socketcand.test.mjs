import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

const source = ts.createSourceFile("main.ts", readFileSync(new URL("../src/main.ts", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true);
const names = ["openChannelDialog", "saveChannelDialog", "updateChannelInterfaceInDialog",
    "setBitrateInDialog", "getBitrateFromDialog", "setListenOnlyInDialog", "getListenOnlyFromDialog",
    "setProtocolInDialog", "getProtocolFromDialog", "setDbcLabel"];
const code = ts.transpileModule(source.statements.filter(n => ts.isFunctionDeclaration(n) && names.includes(n.name?.text))
    .map(n => n.getText(source)).join("\n"), {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.None },
}).outputText;

function harness(ifaces = []) {
    const elements = new Map(), saved = [];
    function element(id) {
        if (!elements.has(id)) elements.set(id, {
            value: "", checked: false, style: {}, textContent: "", open: false,
            set innerHTML(html) {
                this.html = html;
                this.value = html.match(/<option value="([^"]*)"/)?.[1] ?? "";
            },
            get innerHTML() { return this.html; },
            insertAdjacentHTML(_, html) {
                if (!this.value) this.value = html.match(/<option value="([^"]*)"/)?.[1] ?? "";
            },
            addEventListener() {}, contains() { return false; },
            showModal() { this.open = true; }, close() { this.open = false; },
        });
        return elements.get(id);
    }
    const context = vm.createContext({
        document: { activeElement: null, getElementById: element },
        dialogMode: "add", dialogEditTarget: null, dialogPendingDbc: null,
        availableIfaces: [], channels: new Map(), ghostChannels: [], selectedChannel: null,
        escapeHtml: text => text, channelName: () => "Remote", selectChannel() {},
        invoke: async command => {
            if (command === "list_can_interfaces") return ifaces;
            if (command === "set_channel_display_name") return;
            assert.fail(command);
        },
        registerChannel: async config => { saved.push(config); return { handle: 7 }; },
        changeChannelAssignment: async () => true, loadChannelDbc: async () => {},
        restoreProjectEntries: async () => {}, refreshChannelList() {}, rebuildTraceColumns() {},
        renderDbcTree() {}, renderSimEntries() {}, scheduleAutoSave() {},
        log: (level, message) => { if (level === "error") assert.fail(message); },
    });
    vm.runInContext(code, context);
    return { context, element, saved };
}

test("adding Socketcand without hardware enables sending and saves an unspecified bitrate", async () => {
    const h = harness();
    await h.context.openChannelDialog("add");
    assert.equal(h.element("select-iface").value, "__socketcand__");
    assert.equal(h.element("row-socketcand").style.display, "");
    assert.equal(h.element("row-bitrate").style.display, "none");
    assert.equal(h.element("input-listen-only").checked, false);
    assert.match(h.element("label-listen-only").textContent, /server ACK settings unchanged/);
    h.element("input-socketcand").value = "can://127.0.0.1:29537/vcan0";
    await h.context.saveChannelDialog();
    assert.equal(h.saved.length, 1);
    assert.equal(h.saved[0].backend, "socketcand");
    assert.equal(h.saved[0].bitrate, null);
    assert.equal(h.saved[0].listen_only, false);
    assert.equal(h.element("dialog-channel").open, false);
});

test("switching to Socketcand and back preserves the chosen physical bitrate and listen-only flag", async () => {
    const h = harness([{ backend: "pcan", name: "USB" }]);
    await h.context.openChannelDialog("add");
    h.element("select-bitrate").value = "250000";
    for (const listenOnly of [false, true]) {
        h.element("input-listen-only").checked = listenOnly;
        h.element("select-iface").value = "__socketcand__";
        h.element("select-iface").onchange();
        assert.equal(h.element("row-bitrate").style.display, "none");
        assert.equal(h.element("input-bitrate-custom").disabled, true);
        h.element("select-iface").value = "USB";
        h.element("select-iface").onchange();
        assert.equal(h.element("row-socketcand").style.display, "none");
        assert.equal(h.element("row-bitrate").style.display, "");
        assert.equal(h.context.getBitrateFromDialog(), 250000);
        assert.equal(h.element("input-bitrate-custom").disabled, false);
        assert.equal(h.element("input-listen-only").checked, listenOnly);
        assert.match(h.element("label-listen-only").textContent, /no ACKs/);
    }
});

test("previously registered Socketcand addresses use the same remote controls", async () => {
    const name = "can://localhost:29537/vcan0";
    const h = harness([{ backend: "socketcand", name }]);
    await h.context.openChannelDialog("add");
    assert.equal(h.element("row-socketcand").style.display, "none");
    assert.equal(h.element("row-bitrate").style.display, "none");
    await h.context.saveChannelDialog();
    assert.equal(h.saved[0].name, name);
    assert.equal(h.saved[0].backend, "socketcand");
    assert.equal(h.saved[0].bitrate, null);
});

test("editing a saved monitoring channel preserves receive-only until explicitly cleared", async () => {
    const name = "can://localhost:29537/vcan0";
    const h = harness([{ backend: "pcan", name: "USB" }, { backend: "socketcand", name }]);
    const channel = { info: { backend: "socketcand", name }, config: {
        backend: "socketcand", name, bitrate: null, listen_only: true, dbc_path: null, protocol: null,
    } };
    h.context.channels.set(7, channel);
    await h.context.openChannelDialog("edit", 7);
    assert.equal(h.element("row-bitrate").style.display, "none");
    assert.equal(h.element("input-listen-only").checked, true);
    h.element("select-iface").value = "0";
    h.element("select-iface").onchange();
    assert.equal(h.element("row-bitrate").style.display, "");
    h.element("select-iface").value = "1";
    h.element("select-iface").onchange();
    assert.equal(h.element("row-bitrate").style.display, "none");
    h.element("input-listen-only").checked = false;
    await h.context.saveChannelDialog();
    assert.equal(channel.config.listen_only, false);
    assert.equal(channel.config.bitrate, null);
});

test("local virtual interfaces still use the virtual bitrate selection", async () => {
    const h = harness([{ backend: "socketcan", name: "vcan0" }]);
    await h.context.openChannelDialog("add");
    assert.equal(h.element("row-bitrate").style.display, "");
    assert.equal(h.context.getBitrateFromDialog(), null);
    await h.context.saveChannelDialog();
    assert.equal(h.saved[0].backend, "socketcan");
    assert.equal(h.saved[0].bitrate, null);
});
