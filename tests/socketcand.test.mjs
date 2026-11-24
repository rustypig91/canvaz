import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

const source = ts.createSourceFile("main.ts", readFileSync(new URL("../src/main.ts", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true);
const names = ["openChannelDialog", "saveChannelDialog", "updateChannelInterfaceInDialog",
    "setBitrateInDialog", "getBitrateFromDialog", "setListenOnlyInDialog", "getListenOnlyFromDialog",
    "setProtocolInDialog", "getProtocolFromDialog", "setDbcLabel", "setSocketcandAddressError", "removeChannel"];
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
            attributes: new Map(),
            setAttribute(name, value) { this.attributes.set(name, value); },
            focus() { this.focused = true; },
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

test("blank Socketcand addresses show an inline error and block registration until corrected", async () => {
    const h = harness();
    await h.context.openChannelDialog("add");
    const input = h.element("input-socketcand");
    const error = h.element("error-socketcand");

    for (const blank of ["", "   "]) {
        input.value = blank;
        await h.context.saveChannelDialog();
        assert.equal(h.saved.length, 0);
        assert.equal(h.element("dialog-channel").open, true);
        assert.equal(error.textContent, "Enter a Socketcand address.");
        assert.equal(input.attributes.get("aria-invalid"), "true");
        assert.equal(input.focused, true);
    }

    input.value = " can://127.0.0.1:29536/vcan0 ";
    input.oninput();
    assert.equal(error.textContent, "");
    assert.equal(input.attributes.get("aria-invalid"), "false");
    await h.context.saveChannelDialog();
    assert.equal(h.saved.length, 1);
    assert.equal(h.saved[0].name, "can://127.0.0.1:29536/vcan0");
    assert.equal(h.element("dialog-channel").open, false);
});

test("Socketcand registration errors appear beside the address and clear on editing", async () => {
    const h = harness();
    const register = h.context.registerChannel;
    const error = "Socketcand requires an interface, for example can://127.0.0.1:29536/vcan0";
    h.context.registerChannel = async config => config.name === "can://localhost/"
        ? { error } : register(config);
    await h.context.openChannelDialog("add");

    const input = h.element("input-socketcand");
    input.value = "can://localhost/";
    await h.context.saveChannelDialog();
    assert.equal(h.element("error-socketcand").textContent, error);
    assert.equal(input.attributes.get("aria-invalid"), "true");
    assert.equal(input.focused, true);
    assert.equal(h.element("dialog-channel").open, true);

    input.value = "can://localhost/vcan0";
    input.oninput();
    assert.equal(h.element("error-socketcand").textContent, "");
    assert.equal(input.attributes.get("aria-invalid"), "false");
    await h.context.saveChannelDialog();
    assert.equal(h.saved[0].name, input.value);
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

test("editing a Socketcand address keeps the channel and reports invalid replacements inline", async () => {
    const name = "can://localhost/vcan0";
    const h = harness([{ backend: "socketcand", name }]);
    const channel = { info: { backend: "socketcand", name }, config: {
        backend: "socketcand", name, bitrate: null, listen_only: true, dbc_path: null, protocol: null,
    } };
    h.context.channels.set(7, channel);
    h.context.changeChannelAssignment = async (handle, target, reportError) => {
        assert.equal(handle, 7);
        assert.equal(target.backend, "socketcand");
        if (target.name === "can://localhost/") {
            reportError("Socketcand requires an interface");
            return false;
        }
        channel.info = target;
        channel.config.name = target.name;
        return true;
    };

    await h.context.openChannelDialog("edit", 7);
    const input = h.element("input-socketcand");
    assert.equal(input.value, name);
    assert.equal(h.element("row-socketcand").style.display, "");
    input.value = "can://localhost/";
    await h.context.saveChannelDialog();
    assert.equal(h.element("error-socketcand").textContent, "Socketcand requires an interface");
    assert.equal(h.element("dialog-channel").open, true);
    assert.equal(channel.info.name, name);

    input.value = " can://server:30000/can1 ";
    input.oninput();
    await h.context.saveChannelDialog();
    assert.equal(h.context.channels.get(7), channel);
    assert.equal(channel.config.name, "can://server:30000/can1");
    assert.equal(channel.config.listen_only, true);
    assert.equal(h.element("dialog-channel").open, false);
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

test("removing a Socketcand channel keeps it configured until backend removal succeeds", async () => {
    const h = harness();
    const name = "can://localhost/vcan0";
    h.context.channels.set(7, { info: { backend: "socketcand", name } });
    h.context.confirmAndStop = async () => true;
    h.context.log = () => {};
    h.context.renderChannelList = () => {};
    let fail = true;
    h.context.invoke = async (command, args) => {
        assert.equal(command, "remove_channel");
        assert.equal(args.channelHandle, 7);
        assert.equal(h.context.channels.has(7), true);
        if (fail) throw new Error("Channel is open");
    };
    await h.context.removeChannel(7, name);
    assert.equal(h.context.channels.has(7), true);
    fail = false;
    await h.context.removeChannel(7, name);
    assert.equal(h.context.channels.has(7), false);
});
