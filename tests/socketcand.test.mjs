import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

const source = ts.createSourceFile("main.ts", readFileSync(new URL("../src/main.ts", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true);
const names = ["openChannelDialog", "saveChannelDialog", "updateChannelInterfaceInDialog",
    "setBitrateInDialog", "getBitrateFromDialog", "setChannelTimingInDialog", "updateChannelTimingInDialog", "setListenOnlyInDialog", "getListenOnlyFromDialog",
    "setProtocolInDialog", "getProtocolFromDialog", "setDbcLabel", "setSocketcandAddressError", "removeChannel"];
const code = ts.transpileModule(source.statements.filter(n => ts.isFunctionDeclaration(n) && names.includes(n.name?.text))
    .map(n => n.getText(source)).join("\n"), {
    compilerOptions: { target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.None },
}).outputText;

function harness(ifaces = []) {
    const elements = new Map(), saved = [];
    function element(id) {
        if (!elements.has(id)) elements.set(id, {
            value: "", options: [{}, {}],
            get valueAsNumber() { return this.value === "" ? NaN : Number(this.value); },
            checked: false, style: {}, textContent: "", open: false,
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
        dialogExternalTiming: false, dialogCanMode: "classical", dialogMode: "add", dialogEditTarget: null, dialogPendingDbc: null,
        availableIfaces: [], channels: new Map(), ghostChannels: [], selectedChannel: null,
        escapeHtml: text => text, channelName: () => "Remote", selectChannel() {},
        invoke: async command => {
            if (command === "list_can_interfaces") return ifaces;
            if (command === "set_channel_display_name") return;
            assert.fail(command);
        },
        registerChannel: async config => { saved.push(config); return { handle: 7 }; },
        changeChannelAssignment: async () => true, loadChannelDbc: async () => {}, reconcileSimMessages() {},
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
    assert.equal(h.element("select-can-mode").value, "classical");
    assert.equal(h.element("row-data-bitrate").hidden, true);
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
    h.element("select-bitrate").value = "custom";
    h.element("input-bitrate-custom").value = "250000";
    for (const listenOnly of [false, true]) {
        h.element("input-listen-only").checked = listenOnly;
        h.element("select-iface").value = "__socketcand__";
        h.element("select-iface").onchange();
        assert.equal(h.element("row-bitrate").style.display, "none");
        assert.equal(h.element("input-bitrate-custom").disabled, true);
        h.element("select-iface").value = "0";
        h.element("select-iface").onchange();
        assert.equal(h.element("row-socketcand").style.display, "none");
        assert.equal(h.element("row-bitrate").style.display, "");
        assert.equal(h.context.getBitrateFromDialog(), 250000);
        assert.equal(h.element("input-bitrate-custom").disabled, false);
        assert.equal(h.element("input-listen-only").checked, listenOnly);
        assert.match(h.element("label-listen-only").textContent, /no ACKs/);
    }
});

test("switching between FD, virtual, and Classical-only interfaces keeps the physical timing draft", async () => {
    const h = harness([
        { backend: "pcan", name: "CAN 1", capabilities: { fd: true, external_timing: false } },
        { backend: "kvaser", name: "CAN 1", capabilities: { fd: false, external_timing: false } },
        { backend: "socketcan", name: "testbus", capabilities: { fd: true, external_timing: true } },
    ]);
    await h.context.openChannelDialog("add");
    const iface = h.element("select-iface");
    const mode = h.element("select-can-mode");
    const selectInterface = value => {
        iface.value = value;
        iface.onchange();
    };
    mode.value = "fd";
    h.element("select-bitrate").value = "custom";
    h.element("input-bitrate-custom").value = "800000";
    h.element("input-data-bitrate").value = "4000000";

    selectInterface("2");
    assert.equal(mode.value, "fd");
    assert.equal(h.element("row-data-bitrate").hidden, true);
    selectInterface("__socketcand__");
    assert.equal(mode.value, "fd");
    assert.equal(mode.options[1].disabled, false);
    assert.equal(h.element("row-data-bitrate").hidden, true);
    selectInterface("0");
    assert.equal(mode.value, "fd");
    assert.equal(h.element("row-data-bitrate").hidden, false);
    assert.equal(h.context.getBitrateFromDialog(), 800000);
    assert.equal(h.element("input-data-bitrate").value, "4000000");
    selectInterface("1");
    assert.equal(mode.value, "classical");
    await h.context.saveChannelDialog();
    assert.equal(h.saved[0].backend, "kvaser");
    assert.equal(h.saved[0].mode, "classical");
    assert.equal(h.saved[0].data_bitrate, null);
});

test("Socketcand can save an FD channel with server-configured timing", async () => {
    const h = harness([]);
    await h.context.openChannelDialog("add");
    const mode = h.element("select-can-mode");
    assert.equal(mode.value, "classical");
    assert.equal(mode.options[1].disabled, false);
    assert.match(h.element("hint-can-mode").textContent, /server and bus with FD support/);
    mode.value = "fd";
    h.element("input-socketcand").value = "can://localhost:29536/can0";
    await h.context.saveChannelDialog();
    assert.equal(h.saved[0].backend, "socketcand");
    assert.equal(h.saved[0].mode, "fd");
    assert.equal(h.saved[0].bitrate, null);
    assert.equal(h.saved[0].data_bitrate, null);
});

test("editing a saved FD channel preserves timing if its driver stops reporting FD support", async () => {
    const iface = { backend: "pcan", name: "USB", capabilities: { fd: false, external_timing: false, fd_limitation: "This adapter does not support CAN FD" } };
    const h = harness([iface]);
    const channel = { info: iface, config: { backend: iface.backend, name: iface.name, mode: "fd", bitrate: 500000, data_bitrate: 4000000 } };
    h.context.channels.set(7, channel);

    await h.context.openChannelDialog("edit", 7);
    assert.equal(h.element("select-can-mode").value, "fd");
    assert.equal(h.element("hint-can-mode").textContent, iface.capabilities.fd_limitation);
    assert.equal(h.element("input-data-bitrate").value, "4000000");
    h.element("input-channel-name").value = "Test bench";
    await h.context.saveChannelDialog();

    assert.equal(channel.config.display_name, "Test bench");
    assert.equal(channel.config.mode, "fd");
    assert.equal(channel.config.data_bitrate, 4000000);
});

test("editing a channel reconciles simulation definitions after loading its DBC", async () => {
    const iface = { backend: "socketcan", name: "can0" };
    const h = harness([iface]);
    const channel = { info: iface, config: { ...iface, bitrate: 500000, dbc_path: "classic.dbc" } };
    h.context.channels.set(7, channel);
    const replacement = { messages: { 291: { frame_format: "fd" } } };
    const updates = [];
    h.context.loadChannelDbc = async handle => {
        assert.equal(handle, 7);
        assert.equal(channel.config.dbc_path, "mixed.dbc");
        channel.dbc = replacement;
    };
    h.context.reconcileSimMessages = handle => {
        assert.equal(handle, 7);
        assert.equal(channel.dbc, replacement);
        updates.push("reconciled");
    };
    h.context.restoreProjectEntries = async () => { updates.push("restored"); };

    await h.context.openChannelDialog("edit", 7);
    h.context.dialogPendingDbc = "mixed.dbc";
    await h.context.saveChannelDialog();
    assert.deepEqual(updates, ["reconciled", "restored"]);
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

test("switching interfaces while editing preserves the Socketcand address draft", async () => {
    const name = "can://localhost/can0";
    const edited = "can://localhost/can1";
    const h = harness([{ backend: "pcan", name: "USB" }]);
    h.context.channels.set(7, {
        info: { backend: "socketcand", name },
        config: { backend: "socketcand", name, bitrate: null },
    });
    await h.context.openChannelDialog("edit", 7);

    const select = h.element("select-iface");
    const input = h.element("input-socketcand");
    const originalSelection = select.value;
    assert.equal(input.value, name);
    input.value = edited;

    select.value = "0";
    select.onchange();
    assert.equal(h.element("row-socketcand").style.display, "none");
    select.value = "__socketcand__";
    select.onchange();
    assert.equal(input.value, "");
    select.value = originalSelection;
    select.onchange();
    assert.equal(input.value, edited);
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

test("local virtual interfaces use externally managed timing", async () => {
    const h = harness([{ backend: "socketcan", name: "vcan0" }]);
    await h.context.openChannelDialog("add");
    assert.equal(h.element("row-bitrate").style.display, "none");
    assert.match(h.element("hint-can-mode").textContent, /operating system/);
    assert.equal(h.context.getBitrateFromDialog(), null);
    await h.context.saveChannelDialog();
    assert.equal(h.saved[0].backend, "socketcan");
    assert.equal(h.saved[0].bitrate, null);
});

test("CAN FD channel settings keep both bitrates and reject invalid data timing", async () => {
    const h = harness([{ backend: "pcan", name: "PCAN-USB FD", capabilities: { fd: true, external_timing: false } }]);
    await h.context.openChannelDialog("add");
    h.element("select-can-mode").value = "fd";
    h.context.updateChannelTimingInDialog();
    assert.equal(h.element("row-data-bitrate").hidden, false);
    h.element("input-data-bitrate").value = "250000";
    await h.context.saveChannelDialog();
    assert.match(h.element("error-can-timing").textContent, /at least as high/);
    assert.equal(h.saved.length, 0);
    h.element("input-data-bitrate").value = "4294967296";
    await h.context.saveChannelDialog();
    assert.match(h.element("error-can-timing").textContent, /4,294,967,295/);
    assert.equal(h.saved.length, 0);
    h.element("input-data-bitrate").value = "2000000";
    await h.context.saveChannelDialog();
    assert.equal(h.saved[0].mode, "fd");
    assert.equal(h.saved[0].bitrate, 500000);
    assert.equal(h.saved[0].data_bitrate, 2000000);
});

test("Classical CAN rejects invalid nominal bitrates before saving", async () => {
    const h = harness([{ backend: "socketcan", name: "can0" }]);
    await h.context.openChannelDialog("add");
    h.element("select-bitrate").value = "custom";
    h.context.updateChannelTimingInDialog();
    const input = h.element("input-bitrate-custom");
    for (const value of ["", "0", "12345.5", "4294967296"]) {
        input.value = value;
        await h.context.saveChannelDialog();
        assert.match(h.element("error-can-timing").textContent, /nominal bitrate/);
        assert.equal(h.saved.length, 0);
        assert.equal(h.element("dialog-channel").open, true);
    }
    input.value = "800000";
    await h.context.saveChannelDialog();
    assert.equal(h.saved[0].bitrate, 800000);
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
