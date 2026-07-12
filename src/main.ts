import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as dialogOpen, save as dialogSave } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
    Chart,
    LineController,
    LineElement,
    PointElement,
    LinearScale,
    Legend,
    Title,
    Tooltip,
    Filler,
} from "chart.js";
import ZoomPlugin from "chartjs-plugin-zoom";

Chart.register(LineController, LineElement, PointElement, LinearScale, Legend, Title, Tooltip, Filler, ZoomPlugin);

// ── Types ─────────────────────────────────────────────────────────────────────

interface DbcSignal {
    name: string;
    message_id: number;
    message_name: string;
    start_bit: number;
    length: number;
    little_endian: boolean;
    signed: boolean;
    factor: number;
    offset: number;
    min: number;
    max: number;
    unit: string;
    enum_values?: SignalEnumValue[];
    // True for a multiplexor switch signal (M in the DBC).
    multiplexor?: boolean;
    // For multiplexed signals (m<v>): the switch value this signal is active
    // for. Absent for plain signals and the switch itself.
    mux_value?: number | null;
    // IEEE float width from SIG_VALTYPE_ (32 or 64); absent = integer signal.
    // Float signals have no integer raw value — no quantization applies.
    float_bits?: number | null;
}

interface SignalEnumValue {
    value: number;
    description: string;
}

interface DbcMessage {
    id: number;
    name: string;
    dlc: number;
    // 29-bit (extended) frame format, from bit 31 of the DBC's raw id. The
    // backend uses it when encoding sends; kept here for display parity.
    is_extended?: boolean;
    signals: DbcSignal[];
    transmitter?: string | null;
}

interface ParsedDbc {
    path: string;
    // Keyed by CAN id (matches the Rust HashMap serialization).
    messages: Record<number, DbcMessage>;
    // All BU_ nodes declared in the DBC, including ones with no messages.
    nodes: string[];
}

// Per-frame decoded signal carried for trace display (value + raw from Rust).
interface FrameSignal {
    name: string;
    value: number;
    raw: number;
    unit: string;
}

interface FrameInfo {
    channel_handle: number;
    can_id: number;
    is_extended: boolean;
    dlc: number;
    data: number[];
    timestamp_ms: number;
    direction: "rx" | "tx";
    message_name: string | null;
    j1939?: J1939Info | null;
    // True if this row is a synthetic frame reassembled from a J1939 TP.CM /
    // TP.DT transfer rather than a frame that actually appeared on the bus.
    reassembled?: boolean;
    signals: FrameSignal[];
}

interface CanFrameEvent {
    channel_handle: number;
    can_id: number;
    is_extended: boolean;
    dlc: number;
    data: number[];
    timestamp_ms: number;
    direction: "rx" | "tx";
    // J1939 breakdown of the identifier; only present on J1939 channels.
    j1939?: J1939Info | null;
    // True if this row is a synthetic frame reassembled from a J1939 TP.CM /
    // TP.DT transfer rather than a frame that actually appeared on the bus.
    reassembled?: boolean;
    // Decoded signals as interleaved [value, raw, value, raw, …] pairs in DBC
    // message signal order. Names/units/message name are derived from the DBC by
    // position — no strings on the per-frame wire (they dominated GC churn).
    // Signals of inactive multiplexer groups arrive as null pairs (NaN in Rust,
    // serialized to null) so positions stay aligned with the DBC signal list.
    signals: (number | null)[];
}

interface J1939Info { pgn: number; priority: number; sa: number; da: number; }

interface PlotSignalEntry { signal_name: string; channel: string; message_id?: number; }
interface PlotPaneConfig {
    signals: PlotSignalEntry[]; interpolation?: string; show_points?: boolean;
    // Manual Y-axis lock; both set = locked, absent = auto-scale.
    y_min?: number | null; y_max?: number | null;
}
interface ChannelInfo { backend: string; name: string; }
// create_channel result: the backend is where the name was actually found —
// the backend searches all of them, so it can differ from the hint we sent.
interface CreatedChannel { handle: number; backend: string; }
// `display_name` is the user-chosen operating name (shown in the UI, trace and
// CSV exports); `name` stays the hardware identity used for lookup.
interface ChannelConfig { name: string; display_name?: string | null; backend: string; dbc_path: string | null; bitrate: number | null; protocol: string | null; }
// Everything the app tracks about one channel, keyed by its u32 handle in `channels`.
interface Channel {
    info: ChannelInfo;       // backend + hardware name (immutable identity)
    config: ChannelConfig;   // user settings: DBC path + bitrate
    dbc: ParsedDbc | null;   // DBC tree, parsed by open_channel; null until opened
    open: boolean;           // hardware currently open?
    error?: string | null;   // last fatal channel error; cleared on successful open
}

// Backend "channel-error" event: fatal means the RX loop died and the channel
// no longer receives; non-fatal is a TX send failure on a live channel.
interface ChannelErrorEvent { channel_handle: number; error: string; fatal: boolean; }
// One simulated message instance; the same message can appear multiple times.
interface SimMessageConfig {
    channel: string; message_id: number; period_ms: number; running?: boolean;
    signals: { name: string; value: number }[];
}

interface SimRawFrameConfig {
    channel: string; can_id: number; is_extended: boolean;
    dlc: number; data: number[]; period_ms: number; running?: boolean;
}

interface TraceFiltersConfig {
    channels?: string[] | null;
    can_ids?: number[] | null;
    msg_names?: string[] | null;
    dir?: string[] | null;
    // J1939 column filters; -1 stands for frames without J1939 info.
    pgns?: number[] | null;
    prios?: number[] | null;
    sas?: number[] | null;
    das?: number[] | null;
    broadcast?: boolean | null;
    dlc_min?: number | null;
    dlc_max?: number | null;
    cycle_min?: number | null;
    cycle_max?: number | null;
    // Length doubles as the "bytes to check" count of the data filter.
    data?: (number | null)[];
    data_format?: string;
    max_rows?: number | null;
}

// Column widths are deliberately not persisted: every session starts at the
// default widths and resizes last only until the app is reloaded.
interface TraceColumnsConfig {
    order?: string[];
    hidden?: string[];
}

interface Project {
    version: number;
    channels: ChannelConfig[];
    plot_panes: PlotPaneConfig[];
    simulate_messages?: SimMessageConfig[];
    simulate_raw_frames?: SimRawFrameConfig[];
    trace_filters?: TraceFiltersConfig;
    trace_columns?: TraceColumnsConfig;
    window_size_sec?: number;
}

// ── Plot pane state ───────────────────────────────────────────────────────────

const PLOT_COLORS = [
    "#3b82f6", "#22c55e", "#f59e0b", "#ef4444",
    "#8b5cf6", "#06b6d4", "#f97316", "#ec4899",
];

interface PlotSeries {
    signalName: string;
    messageName: string;
    messageId: number;
    unit: string;
    color: string;
    channel: number;
    timestamps: number[];       // absolute ms, used for pruning
    data: { x: number; y: number }[];  // x = elapsed seconds from appStartTime
    lastValue: number | null;
    // Copy of `data` taken at pause time and shown while paused; null = not frozen.
    // A copy (not an index) so the live array keeps being pruned during long pauses.
    frozenData: { x: number; y: number }[] | null;
}

interface PlotPane {
    id: string;
    el: HTMLElement;
    chart: Chart;
    series: Map<string, PlotSeries>;   // key: channel::signalName
    interpolation: 'none' | 'linear' | 'smooth';
    showPoints: boolean;
    hoveredDatasetIndex: number | null;
    zoomed: boolean;
    // Manual Y-axis lock; null = auto-scale. Persisted in the project.
    yLock: { min: number; max: number } | null;
    // Measurement cursor positions in data-x (elapsed seconds), snapped to
    // samples. Both null = cursors off. Not persisted (positions are only
    // meaningful within the capture they were placed in).
    cursorA: number | null;
    cursorB: number | null;
}

const plotPanes: PlotPane[] = [];
let paneCounter = 0;
let viewPaused = false;

// Continuous scroll loop — runs every animation frame while the app is live.
// Advances the X-axis window by wall time so the chart scrolls smoothly even
// when signals arrive infrequently. Also redraws any panes with new data.
let scrollRafId: number | null = null;

// s.data[0] is the left-edge anchor (interpolated at cutoffX in pruneOldData).
// cutoffX is computed at prune time, which is slightly behind xScale.min at render
// time, so Chart.js auto-scaling excludes it from Y range. We extend the Y axis
// via suggestedMin/suggestedMax so the anchor's Y value is always visible.
function applyYRange(pane: PlotPane) {
    if (pane.yLock) return; // manual range pinned via options.scales.y.min/max
    let yMin = Infinity, yMax = -Infinity;
    for (const s of pane.series.values()) {
        const arr = s.frozenData ?? s.data;
        if (arr.length === 0) continue;
        const y = arr[0].y;
        if (y < yMin) yMin = y;
        if (y > yMax) yMax = y;
    }
    const yScale = (pane.chart.options.scales as any)["y"];
    if (isFinite(yMin)) yScale.suggestedMin = yMin;
    else delete yScale.suggestedMin;
    if (isFinite(yMax)) yScale.suggestedMax = yMax;
    else delete yScale.suggestedMax;
}

function startScrollLoop() {
    if (scrollRafId !== null) return;
    function tick() {
        if (!appRunning || viewPaused || !plotTabActive) { scrollRafId = null; return; }
        const now = (Date.now() - appStartTime) / 1000;
        for (const pane of plotPanes) {
            if (!pane.zoomed) {
                const xScale = (pane.chart.options.scales as any)["x"];
                xScale.min = Math.max(0, now - windowSizeSec);
                xScale.max = Math.max(windowSizeSec, now);
            }
            applyYRange(pane);
            pane.chart.update();
        }
        scrollRafId = requestAnimationFrame(tick);
    }
    scrollRafId = requestAnimationFrame(tick);
}

// markPaneDirty: used for one-shot updates when the scroll loop is not running
// (app stopped, paused interactive edits). When the loop is active it handles
// all redraws, so this is a no-op in that case.
const dirtyPanes = new Set<PlotPane>();
let rafPending = false;
function markPaneDirty(pane: PlotPane, force = false) {
    if (!force && viewPaused) return; // data ingestion is suppressed while paused; force=true for user-driven changes
    if (!plotTabActive) return; // don't render when plot tab is not visible
    if (scrollRafId !== null) return; // scroll loop redraws every frame
    // Fallback: one-shot update used when the scroll loop is not running
    // (app stopped, or interactive edits while paused).
    dirtyPanes.add(pane);
    if (!rafPending) {
        rafPending = true;
        requestAnimationFrame(() => {
            rafPending = false;
            for (const p of dirtyPanes) {
                dirtyPanes.delete(p);
                if (appRunning && !p.zoomed && !viewPaused) {
                    const now = (Date.now() - appStartTime) / 1000;
                    const xScale = (p.chart.options.scales as any)["x"];
                    xScale.min = Math.max(0, now - windowSizeSec);
                    xScale.max = Math.max(windowSizeSec, now);
                }
                applyYRange(p);
                p.chart.update();
            }
        });
    }
}

let appRunning = false;
let appStartTime = Date.now();
let plotTabActive = false; // trace tab is the default active tab (see index.html)

// Signals/sim entries to restore into panes after the next startApp (DBC comes from open_channel)
let pendingPaneSignals: PlotSignalEntry[][] = [];
// Simulated message instances to restore on next startApp.
let pendingSimMessages: SimMessageConfig[] = [];

// Channels that failed create_channel (hardware not present). Kept so the
// project config is preserved and startApp can retry them.
interface GhostChannel { config: ChannelConfig; error: string; }
let ghostChannels: GhostChannel[] = [];

// Middle-mouse pan state
let midPan: { startX: number; startMin: number; startMax: number; chartWidth: number } | null = null;

// X window shared by all panes just before zooming/panning began, so "reset
// zoom" can restore every pane to it. The zoom plugin only remembers the pane
// the user dragged on; the others get their range mirrored by direct option
// writes, which resetZoom() knows nothing about.
let preZoomX: { min: number; max: number } | null = null;


// ── Measurement cursors ───────────────────────────────────────────────────────
// Two vertical cursors (A amber, B blue) per pane, placed on the data's time
// axis so they stay glued to their samples while the view scrolls or zooms.
// Values and Δs are shown in a readout overlay; lines are drawn by a chart
// plugin so they survive every Chart.js redraw.

const CURSOR_COLOR_A = "#facc15";
const CURSOR_COLOR_B = "#38bdf8";
const CURSOR_GRAB_PX = 6;

// The array a series currently renders from (frozen copy while paused).
function seriesViewData(s: PlotSeries): { x: number; y: number }[] {
    return viewPaused && s.frozenData ? s.frozenData : s.data;
}

// Index of the sample nearest to x (arrays are sorted by x).
function nearestSampleIdx(arr: { x: number; y: number }[], x: number): number {
    let lo = 0, hi = arr.length - 1;
    while (lo < hi) {
        const mid = (lo + hi) >> 1;
        if (arr[mid].x < x) lo = mid + 1; else hi = mid;
    }
    if (lo > 0 && Math.abs(arr[lo - 1].x - x) < Math.abs(arr[lo].x - x)) return lo - 1;
    return lo;
}

// Nearest sample x across all series of the pane; null when the pane is empty.
function snapToNearestSample(pane: PlotPane, x: number): number | null {
    let best: number | null = null;
    let bestDist = Infinity;
    for (const s of pane.series.values()) {
        const arr = seriesViewData(s);
        if (!arr.length) continue;
        const sx = arr[nearestSampleIdx(arr, x)].x;
        const d = Math.abs(sx - x);
        if (d < bestDist) { bestDist = d; best = sx; }
    }
    return best;
}

// Series value at data-x: the sample nearest to x, or null when empty.
function seriesValueAt(s: PlotSeries, x: number): number | null {
    const arr = seriesViewData(s);
    if (!arr.length) return null;
    return arr[nearestSampleIdx(arr, x)].y;
}

// Draws the cursor lines. Registered globally; charts that aren't plot panes
// (inline trace plots) fail the pane lookup and are left untouched.
const cursorPlugin = {
    id: "canvazCursors",
    afterDatasetsDraw(chart: Chart) {
        const pane = plotPanes.find(p => p.chart === chart);
        if (!pane || (pane.cursorA === null && pane.cursorB === null)) return;
        const area = chart.chartArea;
        const xs = (chart.scales as any)["x"];
        if (!area || !xs) return;
        const ctx = chart.ctx;
        ctx.save();
        for (const [label, x, color] of [["A", pane.cursorA, CURSOR_COLOR_A], ["B", pane.cursorB, CURSOR_COLOR_B]] as const) {
            if (x === null) continue;
            const px = xs.getPixelForValue(x);
            if (px < area.left || px > area.right) continue;
            ctx.strokeStyle = color;
            ctx.lineWidth = 1;
            ctx.setLineDash([4, 3]);
            ctx.beginPath();
            ctx.moveTo(px, area.top);
            ctx.lineTo(px, area.bottom);
            ctx.stroke();
            ctx.setLineDash([]);
            ctx.fillStyle = color;
            ctx.fillRect(px - 7, area.top, 14, 13);
            ctx.fillStyle = "#18181b";
            ctx.font = "bold 9px sans-serif";
            ctx.textAlign = "center";
            ctx.textBaseline = "middle";
            ctx.fillText(label, px, area.top + 7);
        }
        ctx.restore();
    },
};
Chart.register(cursorPlugin as any);

function setPaneCursors(pane: PlotPane, enabled: boolean) {
    const btn = pane.el.querySelector<HTMLButtonElement>(".btn-cursors")!;
    const readout = pane.el.querySelector<HTMLElement>(".cursor-readout")!;
    if (!enabled) {
        pane.cursorA = pane.cursorB = null;
        btn.classList.remove("active");
        readout.style.display = "none";
        markPaneDirty(pane, true);
        return;
    }
    // Start at 30% / 70% of the visible window, snapped to real samples.
    const xs = (pane.chart.scales as any)["x"];
    const a = xs.min + (xs.max - xs.min) * 0.3;
    const b = xs.min + (xs.max - xs.min) * 0.7;
    pane.cursorA = snapToNearestSample(pane, a) ?? a;
    pane.cursorB = snapToNearestSample(pane, b) ?? b;
    btn.classList.add("active");
    readout.style.display = "";
    updateCursorReadout(pane);
    markPaneDirty(pane, true);
}

function fmtCursorDt(dt: number): string {
    return dt >= 1 ? `${dt.toFixed(3)} s` : `${(dt * 1000).toFixed(1)} ms`;
}

function updateCursorReadout(pane: PlotPane) {
    const readout = pane.el.querySelector<HTMLElement>(".cursor-readout")!;
    const { cursorA: a, cursorB: b } = pane;
    if (a === null || b === null) return;
    const dt = Math.abs(b - a);
    const freq = dt > 0 ? ` · ${(1 / dt).toFixed(1 / dt >= 100 ? 0 : 2)} Hz` : "";
    let html = `<div class="cr-head">Δt ${fmtCursorDt(dt)}${freq}</div>`;
    html += `<table><tr><th></th><th style="color:${CURSOR_COLOR_A}">A</th><th style="color:${CURSOR_COLOR_B}">B</th><th>Δ</th></tr>`;
    for (const s of pane.series.values()) {
        const va = seriesValueAt(s, a);
        const vb = seriesValueAt(s, b);
        const f = (v: number | null) => v === null ? "—" : formatSigValue(v, "");
        const dv = va !== null && vb !== null
            ? `${vb - va >= 0 ? "+" : ""}${formatSigValue(vb - va, s.unit)}`
            : "—";
        html += `<tr><td><span class="cr-dot" style="background:${s.color}"></span>${s.signalName}</td><td>${f(va)}</td><td>${f(vb)}</td><td>${dv}</td></tr>`;
    }
    readout.innerHTML = html + "</table>";
}

// ── Y-axis lock ───────────────────────────────────────────────────────────────

function setYLock(pane: PlotPane, lock: { min: number; max: number } | null, save = true) {
    pane.yLock = lock;
    const yScale = (pane.chart.options.scales as any)["y"];
    const btn = pane.el.querySelector<HTMLButtonElement>(".btn-ylock")!;
    const wrap = pane.el.querySelector<HTMLElement>(".ylock-inputs")!;
    if (lock) {
        yScale.min = lock.min;
        yScale.max = lock.max;
        delete yScale.suggestedMin;
        delete yScale.suggestedMax;
        btn.classList.add("active");
        btn.title = "Unlock Y axis (auto-scale)";
        wrap.style.display = "";
        wrap.querySelector<HTMLInputElement>(".ylock-min")!.value = fmtNum(lock.min);
        wrap.querySelector<HTMLInputElement>(".ylock-max")!.value = fmtNum(lock.max);
    } else {
        delete yScale.min;
        delete yScale.max;
        btn.classList.remove("active");
        btn.title = "Lock Y axis to current range";
        wrap.style.display = "none";
    }
    markPaneDirty(pane, true);
    if (save) scheduleAutoSave("plot y-lock changed");
}

// ── PNG export ────────────────────────────────────────────────────────────────

// Composite the chart canvas onto an opaque background (the chart itself is
// transparent) and save it via the backend.
async function exportPanePng(pane: PlotPane) {
    const src = pane.chart.canvas;
    const out = document.createElement("canvas");
    out.width = src.width;
    out.height = src.height;
    const ctx = out.getContext("2d")!;
    const bg = getComputedStyle(pane.el).backgroundColor;
    ctx.fillStyle = bg && bg !== "rgba(0, 0, 0, 0)" ? bg : "#1b1c20";
    ctx.fillRect(0, 0, out.width, out.height);
    ctx.drawImage(src, 0, 0);
    const names = [...pane.series.values()].map(s => s.signalName).join("_") || "plot";
    const safeName = names.replace(/[\\/:*?"<>|]/g, "_").slice(0, 80);
    const path = await dialogSave({
        defaultPath: `${safeName}.png`,
        filters: [{ name: "PNG image", extensions: ["png"] }],
    });
    if (!path) return;
    const blob = await new Promise<Blob | null>(res => out.toBlob(res, "image/png"));
    if (!blob) { log("error", "Failed to encode PNG"); return; }
    try {
        await invoke("write_binary_file", { path, data: Array.from(new Uint8Array(await blob.arrayBuffer())) });
        log("info", `Saved plot image: ${path}`);
    } catch (e) {
        log("error", `Failed to save PNG: ${e}`);
    }
}

function plotKey(channel: number, messageId: number, signalName: string) {
    return `${channel}::${messageId}::${signalName}`;
}

// plotKey strings for every signal of a message, built once per (channel,
// message) and reused. Building them per signal per frame dominated GC churn at
// high frame rates. Invalidated whenever a channel's DBC is (re)loaded.
const sigKeyCache = new Map<number, Map<number, string[]>>();
function sigKeysFor(handle: number, msg: DbcMessage): string[] {
    let byMsg = sigKeyCache.get(handle);
    if (!byMsg) { byMsg = new Map(); sigKeyCache.set(handle, byMsg); }
    let keys = byMsg.get(msg.id);
    if (!keys) {
        keys = msg.signals.map(s => plotKey(handle, msg.id, s.name));
        byMsg.set(msg.id, keys);
    }
    return keys;
}

// ── J1939 helpers ─────────────────────────────────────────────────────────────

// 18-bit PGN of a 29-bit identifier (EDP/DP included; PS folded in for PDU2).
function j1939Pgn(canId: number): number {
    const pf = (canId >> 16) & 0xFF;
    let pgn = (canId >> 8) & 0x3ff00;
    if (pf >= 240) pgn |= (canId >> 8) & 0xFF;
    return pgn;
}

// PDU2 (PF ≥ 240) parameter groups are broadcast; PDU1 are destination-specific.
function j1939IsBroadcast(pgn: number): boolean {
    return ((pgn >> 8) & 0xFF) >= 240;
}

function fmtPgn(pgn: number): string {
    return pgn.toString(16).toUpperCase().padStart(4, "0") + "h";
}

function fmtJ1939Addr(addr: number): string {
    return addr.toString(16).toUpperCase().padStart(2, "0") + "h";
}

// Per-channel (PGN, source address) → DBC message map for J1939 channels, so
// frames match their DBC message regardless of the priority bits (and, for
// PDU1 groups, the destination address) in the wire id. The source address
// stays part of the identity — the same PGN from two nodes can be two
// different DBC messages. Keys are (pgn << 8) | sa. Built lazily; invalidated
// alongside sigKeyCache whenever a DBC is reloaded.
const pgnMapCache = new Map<number, Map<number, DbcMessage>>();

function j1939MatchKey(canId: number): number {
    return (j1939Pgn(canId) << 8) | (canId & 0xFF);
}

// DBC message for a frame: exact id match first, then (on J1939 channels) by
// PGN + source address.
function dbcMessageFor(handle: number, canId: number, isExtended: boolean): DbcMessage | null {
    const ch = channels.get(handle);
    const dbc = ch?.dbc;
    if (!dbc) return null;
    const exact = dbc.messages[canId];
    if (exact) return exact;
    if (ch!.config.protocol !== "j1939" || !isExtended) return null;
    let map = pgnMapCache.get(handle);
    if (!map) {
        map = new Map();
        for (const m of Object.values(dbc.messages)) {
            if (m.id > 0x7FF) map.set(j1939MatchKey(m.id), m);
        }
        pgnMapCache.set(handle, map);
    }
    return map.get(j1939MatchKey(canId)) ?? null;
}

function formatSigValue(value: number, unit: string): string {
    const abs = Math.abs(value);
    const s = abs >= 10000 ? value.toFixed(0)
        : abs >= 100 ? value.toFixed(1)
            : abs >= 10 ? value.toFixed(2)
                : value.toFixed(3);
    return unit ? `${s} ${unit}` : s;
}

// ── Pane lifecycle ────────────────────────────────────────────────────────────

function updatePaneTitle(pane: PlotPane) {
    const names = [...pane.series.values()].map(s => s.signalName);
    const title = pane.el.querySelector<HTMLElement>(".pane-title")!;
    title.textContent = names.length ? names.join(", ") : `Plot ${pane.id.replace("pane-", "")}`;
}

function clearPaneZoom(pane: PlotPane) {
    preZoomX = null;
    if (!pane.zoomed) return;
    const zoomOpts = (pane.chart.options.plugins as any).zoom.zoom;
    const saved = zoomOpts.onZoomComplete;
    delete zoomOpts.onZoomComplete;
    pane.chart.resetZoom();
    zoomOpts.onZoomComplete = saved;
    pane.zoomed = false;
    pane.el.querySelector<HTMLButtonElement>(".btn-reset-zoom")!.style.display = "none";
}

// Reset zoom on ALL panes, restoring the x window they showed before zooming
// began. resetZoom() alone only restores the pane the user dragged on (the
// plugin saved its pre-zoom range); mirrored panes must be restored explicitly.
function resetAllZoom() {
    const restore = preZoomX;
    const now = (Date.now() - appStartTime) / 1000;
    const min = restore ? restore.min : Math.max(0, now - windowSizeSec);
    const max = restore ? restore.max : Math.max(windowSizeSec, now);
    for (const pane of plotPanes) {
        clearPaneZoom(pane);
        const xScale = (pane.chart.options.scales as any)["x"];
        xScale.min = min;
        xScale.max = max;
        pane.chart.update();
    }
}

function snapshotPlotPanes() {
    for (const pane of plotPanes) {
        const seriesArray = [...pane.series.values()];
        for (let i = 0; i < seriesArray.length; i++) {
            const s = seriesArray[i];
            s.frozenData = s.data.slice();
            // Point the chart at the frozen copy immediately. Later update()
            // calls during the pause (second zoom, mirrored ranges, mid-pan)
            // would otherwise re-read the live array, which pruning keeps
            // draining — the zoomed-in region empties and its points vanish.
            const ds = pane.chart.data.datasets[i] as any;
            if (ds) ds.data = s.frozenData;
        }
        // Update NOW so Chart.js rebinds its array-mutation listeners to the
        // frozen copy. Otherwise it keeps listening to the live array and
        // queues element syncs against the wrong (frozen) length while ingest
        // pushes and pruning splices during the pause; if the pause ends with
        // ds.data set back to the same live reference, that stale queue is
        // applied and punches undefined holes into the element array — the
        // next chart.update() then throws "Cannot set properties of
        // undefined (setting 'x')".
        pane.chart.update("none");
    }
    // Freeze the sidebar value maps too, so signal rows built during the pause
    // match the frozen view instead of showing live values.
    sidebarSnapshot = {
        values: new Map(signalLastValues),
        raw: new Map(signalLastRaw),
        min: new Map(signalMinValues),
        max: new Map(signalMaxValues),
    };
}

function removeSigFromPane(pane: PlotPane, key: string) {
    if (!pane.series.delete(key)) return;
    if (pane.series.size === 0) { closePlotPane(pane.id); return; }
    syncDatasets(pane);
    updatePaneTitle(pane);
    updateSignalHighlights();
    scheduleAutoSave("plot signal removed");
}

function createPlotPane(): PlotPane {
    const id = `pane-${++paneCounter}`;

    // Allocate pane object first so legend callbacks can close over it
    const pane: PlotPane = { id, el: null!, chart: null!, series: new Map(), interpolation: 'none', showPoints: false, hoveredDatasetIndex: null, zoomed: false, yLock: null, cursorA: null, cursorB: null };

    const el = document.createElement("div");
    el.className = "plot-pane";
    el.id = id;
    el.dataset.paneId = id;
    el.innerHTML = `
    <div class="pane-header">
      <span class="pane-title">Plot ${paneCounter}</span>
      <button class="btn-reset-zoom pane-btn" title="Reset zoom" style="display:none">⟲</button>
      <button class="btn-show-points pane-btn" title="Show data points: off">•</button>
      <select class="sel-interp" title="Interpolation">
        <option value="none">None</option>
        <option value="linear">Linear</option>
        <option value="smooth">Smooth</option>
      </select>
      <button class="btn-cursors pane-btn" title="Measurement cursors">⌖</button>
      <button class="btn-ylock pane-btn" title="Lock Y axis to current range">Y</button>
      <span class="ylock-inputs" style="display:none">
        <input type="number" step="any" class="ylock-min" title="Y axis minimum">
        <input type="number" step="any" class="ylock-max" title="Y axis maximum">
      </span>
      <button class="btn-export-png pane-btn" title="Save pane as PNG">⤓</button>
      <button class="btn-close-pane" title="Close plot">×</button>
    </div>
    <div class="pane-canvas-wrap">
      <canvas></canvas>
      <div class="cursor-readout" style="display:none"></div>
    </div>
  `;
    el.querySelector(".btn-close-pane")!.addEventListener("click", () => closePlotPane(id));
    el.querySelector<HTMLButtonElement>(".btn-show-points")!.addEventListener("click", (e) => {
        pane.showPoints = !pane.showPoints;
        const btn = e.currentTarget as HTMLButtonElement;
        btn.classList.toggle("active", pane.showPoints);
        btn.title = `Show data points: ${pane.showPoints ? "on" : "off"}`;
        syncDatasets(pane);
        scheduleAutoSave("plot show-points toggled");
    });
    el.querySelector<HTMLSelectElement>(".sel-interp")!.addEventListener("change", (e) => {
        pane.interpolation = (e.currentTarget as HTMLSelectElement).value as PlotPane["interpolation"];
        syncDatasets(pane);
        scheduleAutoSave("plot interpolation changed");
    });
    const resetZoomBtn = el.querySelector<HTMLButtonElement>(".btn-reset-zoom")!;
    resetZoomBtn.addEventListener("click", () => resetAllZoom());

    el.querySelector<HTMLButtonElement>(".btn-cursors")!.addEventListener("click", () => {
        setPaneCursors(pane, pane.cursorA === null && pane.cursorB === null);
    });

    el.querySelector<HTMLButtonElement>(".btn-ylock")!.addEventListener("click", () => {
        if (pane.yLock) { setYLock(pane, null); return; }
        // Pin whatever range auto-scaling currently shows.
        const ys = (pane.chart.scales as any)["y"];
        setYLock(pane, { min: ys.min, max: ys.max });
    });
    const applyYLockInputs = () => {
        const min = parseFloat(el.querySelector<HTMLInputElement>(".ylock-min")!.value);
        const max = parseFloat(el.querySelector<HTMLInputElement>(".ylock-max")!.value);
        if (Number.isFinite(min) && Number.isFinite(max) && min < max) setYLock(pane, { min, max });
    };
    el.querySelector<HTMLInputElement>(".ylock-min")!.addEventListener("change", applyYLockInputs);
    el.querySelector<HTMLInputElement>(".ylock-max")!.addEventListener("change", applyYLockInputs);

    el.querySelector<HTMLButtonElement>(".btn-export-png")!.addEventListener("click", () => { exportPanePng(pane); });

    const canvas = el.querySelector<HTMLCanvasElement>("canvas")!;

    // Which cursor line (if any) is within grabbing distance of a canvas x.
    const cursorHit = (offsetX: number): "a" | "b" | null => {
        const xs = (pane.chart?.scales as any)?.["x"];
        if (!xs) return null;
        const hits: Array<["a" | "b", number]> = [];
        if (pane.cursorA !== null) hits.push(["a", Math.abs(xs.getPixelForValue(pane.cursorA) - offsetX)]);
        if (pane.cursorB !== null) hits.push(["b", Math.abs(xs.getPixelForValue(pane.cursorB) - offsetX)]);
        hits.sort((p, q) => p[1] - q[1]);
        return hits.length && hits[0][1] <= CURSOR_GRAB_PX ? hits[0][0] : null;
    };

    // Cursor dragging. Registered BEFORE the chart is created: at-target
    // listeners fire in registration order, so stopImmediatePropagation()
    // here keeps the zoom plugin's own mousedown from starting a drag-zoom
    // when the user grabs a cursor line.
    canvas.addEventListener("mousedown", (e) => {
        if (e.button !== 0) return;
        const which = cursorHit(e.offsetX);
        if (!which) return;
        e.preventDefault();
        e.stopImmediatePropagation();
        const move = (ev: MouseEvent) => {
            const rect = canvas.getBoundingClientRect();
            const xs = (pane.chart.scales as any)["x"];
            const raw = xs.getValueForPixel(ev.clientX - rect.left);
            const clamped = Math.min(xs.max, Math.max(xs.min, raw));
            const snapped = snapToNearestSample(pane, clamped) ?? clamped;
            if (which === "a") pane.cursorA = snapped; else pane.cursorB = snapped;
            updateCursorReadout(pane);
            markPaneDirty(pane, true);
        };
        const up = () => {
            window.removeEventListener("mousemove", move);
            window.removeEventListener("mouseup", up);
        };
        window.addEventListener("mousemove", move);
        window.addEventListener("mouseup", up);
    });
    canvas.addEventListener("mousemove", (e) => {
        if (pane.cursorA === null && pane.cursorB === null) return;
        canvas.style.cursor = cursorHit(e.offsetX) ? "ew-resize" : "";
    });

    canvas.addEventListener("mousedown", (e) => {
        if (e.button !== 1) return;
        e.preventDefault(); // suppress autoscroll cursor
        const xScale = (pane.chart.scales as any)["x"];
        const area = pane.chart.chartArea;
        if (!area) return;
        midPan = {
            startX: e.clientX,
            startMin: xScale.min,
            startMax: xScale.max,
            chartWidth: area.right - area.left,
        };
        if (preZoomX === null) preZoomX = { min: xScale.min, max: xScale.max };
        // Freeze the view (only meaningful while capture is live and unpaused).
        if (appRunning && !viewPaused) {
            viewPaused = true;
            updatePauseViewBtn();
            snapshotPlotPanes();
        }
        for (const p of plotPanes) {
            p.zoomed = true;
            p.el.querySelector<HTMLButtonElement>(".btn-reset-zoom")!.style.display = "";
        }
    });

    const chart = new Chart(canvas, {
        type: "line",
        data: { datasets: [] },
        options: {
            animation: false,
            responsive: true,
            maintainAspectRatio: false,
            interaction: { mode: "index", intersect: false },
            parsing: false,
            plugins: {
                legend: {
                    display: true,
                    labels: { color: "#e4e4e7", boxWidth: 24, boxHeight: 2, padding: 12 },
                    onHover: (_evt, item) => {
                        pane.hoveredDatasetIndex = item.datasetIndex ?? null;
                        (pane.chart as any).tooltip?.setActiveElements([], { x: 0, y: 0 });
                        syncDatasets(pane);
                    },
                    onLeave: () => {
                        pane.hoveredDatasetIndex = null;
                        syncDatasets(pane);
                    },
                    onClick: (_evt, item) => {
                        const key = [...pane.series.keys()][item.datasetIndex!];
                        if (key) removeSigFromPane(pane, key);
                    },
                },
                zoom: {
                    zoom: {
                        drag: {
                            enabled: true,
                            backgroundColor: "rgba(59,130,246,0.10)",
                            borderColor: "rgba(59,130,246,0.5)",
                            borderWidth: 1,
                        },
                        mode: "x" as const,
                        onZoomStart: () => {
                            if (preZoomX === null) {
                                const xs = (pane.chart.scales as any)["x"];
                                preZoomX = { min: xs.min, max: xs.max };
                            }
                        },
                        onZoomComplete: () => {
                            pane.zoomed = true;
                            resetZoomBtn.style.display = "";
                            // Freeze the view (only meaningful while capture is live).
                            if (appRunning && !viewPaused) {
                                viewPaused = true;
                                updatePauseViewBtn();
                                snapshotPlotPanes();
                            }
                            // Mirror the same x range to every other pane.
                            const zoomedX = (pane.chart.scales as any)["x"];
                            const zMin = zoomedX.min, zMax = zoomedX.max;
                            for (const other of plotPanes) {
                                if (other === pane) continue;
                                const xScale = (other.chart.options.scales as any)["x"];
                                xScale.min = zMin;
                                xScale.max = zMax;
                                other.zoomed = true;
                                other.el.querySelector<HTMLButtonElement>(".btn-reset-zoom")!.style.display = "";
                                other.chart.update();
                            }
                        },
                    },
                },
            },
            scales: {
                x: {
                    type: "linear" as const,
                    min: 0,
                    max: windowSizeSec,
                    afterBuildTicks: (axis: any) => {
                        const range = axis.max - axis.min;
                        const rawStep = range / 6;
                        const mag = Math.pow(10, Math.floor(Math.log10(rawStep)));
                        const n = rawStep / mag;
                        const step = n < 1.5 ? mag : n < 3.5 ? 2 * mag : n < 7.5 ? 5 * mag : 10 * mag;
                        // Fixed-length tick array so Chart.js never sees a length change and
                        // skips the expensive layout pass. Origin snaps to step multiples;
                        // off-screen buffer slots absorb the discrete shift so visible lines
                        // enter at axis.max and exit at axis.min without any count change.
                        const stepCount = Math.ceil(range / step) + 2;
                        const origin = Math.ceil(axis.min / step) * step;
                        const ticks: { value: number }[] = [];
                        for (let i = 0; i < stepCount; i++) ticks.push({ value: origin + i * step });
                        axis.ticks = ticks;
                    },
                    ticks: {
                        color: "#71717a",
                        maxTicksLimit: 8,
                        maxRotation: 0,
                        callback: function (v, index, ticks) {
                            const s = typeof v === "number" ? v : parseFloat(String(v));
                            const range = this.max - this.min;
                            // Suppress the label while the line is within half a step of the right
                            // edge. The grid line enters at axis.max first; the label appears once
                            // it has scrolled far enough that its text fits without overflowing —
                            // which is what was forcing Chart.js to add right-padding and stutter.
                            const step = ticks.length >= 2 ? ticks[1].value - ticks[0].value : range;
                            if (s > this.max - step * 0.5) return null;
                            const label = range < 1 ? `${Math.round(s * 1000)}ms` : `${Math.round(s)}s`;
                            if (index > 0) {
                                const prev = ticks[index - 1].value;
                                const prevLabel = range < 1 ? `${Math.round(prev * 1000)}ms` : `${Math.round(prev)}s`;
                                if (label === prevLabel) return null;
                            }
                            return label;
                        },
                    },
                    grid: { color: "#2a2b30" },
                },
                y: { border: { display: true, color: "#2a2b30" }, ticks: { color: "#71717a" }, grid: { color: "#2a2b30" } },
            },
        },
    });

    pane.el = el;
    pane.chart = chart;

    const container = document.getElementById("plot-panes-container")!;
    container.insertBefore(el, document.getElementById("drop-zone-new"));
    plotPanes.push(pane);
    setupPaneDrop(el, pane);
    return pane;
}

function closePlotPane(id: string) {
    const idx = plotPanes.findIndex(p => p.id === id);
    if (idx === -1) return;
    const [pane] = plotPanes.splice(idx, 1);
    pane.chart.destroy();
    pane.el.remove();
    updateSignalHighlights();
    scheduleAutoSave("plot pane closed");
}

// ── Signal → pane ─────────────────────────────────────────────────────────────

async function addSignalToPane(pane: PlotPane, handle: number, sig: DbcSignal) {
    const key = plotKey(handle, sig.message_id, sig.name);
    if (pane.series.has(key)) return;
    const color = PLOT_COLORS[pane.series.size % PLOT_COLORS.length];
    const series: PlotSeries = {
        signalName: sig.name, messageName: sig.message_name, messageId: sig.message_id, unit: sig.unit,
        color, channel: handle, timestamps: [], data: [], lastValue: null, frozenData: null,
    };
    // Register the series NOW so live signal-value events are captured immediately
    // while we await the history fetch below.
    pane.series.set(key, series);

    // If another pane already loaded history for this signal, copy from it to
    // avoid a redundant backend round-trip.
    let copiedFromExisting = false;
    for (const other of plotPanes) {
        if (other === pane) continue;
        const existing = other.series.get(key);
        if (existing && existing.timestamps.length > 0) {
            series.timestamps = [...existing.timestamps];
            series.data = [...existing.data];
            series.lastValue = existing.lastValue;
            copiedFromExisting = true;
            break;
        }
    }

    if (!copiedFromExisting) {
        // Load history from the backend. Live events that arrived while we awaited
        // are already in series.timestamps (pushed by onSignalValue). Prepend only
        // historical samples that are older than the first live sample.
        try {
            const history = await invoke<Array<{ timestamp_ms: number; value: number }>>(
                "get_signal_history", { handle, messageId: sig.message_id, signalName: sig.name, sinceMs: 0 }
            );
            const liveStart = series.timestamps[0] ?? Infinity;
            const toInsert = history.filter(s => s.timestamp_ms < liveStart);
            if (toInsert.length > 0) {
                series.timestamps = [...toInsert.map(s => s.timestamp_ms), ...series.timestamps];
                series.data = [...toInsert.map(s => ({ x: (s.timestamp_ms - appStartTime) / 1000, y: s.value })), ...series.data];
            }
            if (series.data.length > 0) series.lastValue = series.data[series.data.length - 1].y;
        } catch { /* channel not open or no DBC yet — data will stream in via events */ }
    }

    syncDatasets(pane);
    updatePaneTitle(pane);
    updateSignalHighlights();
    scheduleAutoSave("signal added to plot");
}

// Add every signal of a message to a specific pane (dropped onto that pane).
async function addMessageSignalsToPane(pane: PlotPane, handle: number, msg: DbcMessage) {
    if (!msg.signals.length) { log("warn", `${msg.name} has no signals to plot`); return; }
    for (const sig of msg.signals) await addSignalToPane(pane, handle, sig);
}

// Whole message dropped somewhere with no specific pane target (the "new
// plot" drop zone, or double-click): create a fresh pane for it.
async function addMessageToNewPane(handle: number, msg: DbcMessage) {
    if (!msg.signals.length) { log("warn", `${msg.name} has no signals to plot`); return; }
    await addMessageSignalsToPane(createPlotPane(), handle, msg);
}

function syncDatasets(pane: PlotPane) {
    const tension = pane.interpolation === 'smooth' ? 0.4 : 0;
    const stepped: 'before' | false = pane.interpolation === 'none' ? 'before' : false;
    const seriesArray = [...pane.series.values()];

    // Grow / shrink the datasets array to match the series count, mutating
    // existing objects in-place so Chart.js keeps its internal meta state
    // for each dataset instead of re-initialising and flashing blank.
    while (pane.chart.data.datasets.length > seriesArray.length)
        pane.chart.data.datasets.pop();
    while (pane.chart.data.datasets.length < seriesArray.length)
        pane.chart.data.datasets.push({} as any);

    for (let i = 0; i < seriesArray.length; i++) {
        const s = seriesArray[i];
        const hovered = pane.hoveredDatasetIndex === i;
        const showDot = pane.showPoints || hovered;
        const ds = pane.chart.data.datasets[i] as any;
        ds.label = s.signalName;
        ds.data = viewPaused && s.frozenData ? s.frozenData : s.data;
        ds.borderColor = s.color;
        ds.pointBackgroundColor = s.color;
        ds.backgroundColor = "transparent";
        ds.borderWidth = hovered ? 2.5 : 1.5;
        ds.pointRadius = showDot ? 3 : 0;
        ds.pointHoverRadius = hovered ? 5 : 3;
        ds.tension = tension;
        ds.stepped = stepped;
    }
    // Route through the RAF loop so there is never more than one update()
    // per frame, even when syncDatasets is called synchronously (e.g. hover).
    // force=true: user-driven changes (interpolation, show-points, hover) must
    // redraw even while the view is paused or zoomed.
    markPaneDirty(pane, true);
}


// ── Signal highlights in DBC tree ─────────────────────────────────────────────

function updateSignalHighlights() {
    const plotted = new Set<string>();
    for (const pane of plotPanes)
        for (const key of pane.series.keys()) plotted.add(key);

    // Simulation is per whole message, so track it at the message level
    // (channel::messageId) and indicate it on the message group, not each signal.
    const simulatedMsgs = new Set<string>();
    for (const entry of simEntries.values()) {
        if (entry.kind === "message") simulatedMsgs.add(`${entry.channel}::${entry.messageId}`);
    }

    document.querySelectorAll<HTMLElement>(".signal-row").forEach(row => {
        const key = plotKey(parseInt(row.dataset.channel ?? "0"), parseInt(row.dataset.messageId ?? "0"), row.dataset.signal ?? "");
        row.classList.toggle("in-plot", plotted.has(key));
    });

    document.querySelectorAll<HTMLElement>(".msg-group").forEach(group => {
        const msgKey = `${group.dataset.channel ?? "0"}::${group.dataset.messageId ?? "0"}`;
        group.classList.toggle("in-sim", simulatedMsgs.has(msgKey));
    });
}

// ── DBC tree rendering ────────────────────────────────────────────────────────

// True when any plot pane currently charts this plotKey.
function isKeyPlotted(key: string): boolean {
    for (const pane of plotPanes) if (pane.series.has(key)) return true;
    return false;
}

function renderDbcTree(filter = "") {
    const tree = document.getElementById("dbc-tree")!;
    tree.innerHTML = "";
    signalValueEls.clear();
    signalRangeEls.clear();
    signalEnums.clear();
    signalUnits.clear();

    const dbc = selectedChannel !== null ? (channels.get(selectedChannel)?.dbc ?? null) : null;
    if (!dbc) {
        tree.innerHTML = `<div style="padding:8px 12px;color:var(--text-muted);font-size:11px">${selectedChannel !== null ? "No DBC loaded for this channel" : "Select a channel"
            }</div>`;
        return;
    }

    const lc = filter.toLowerCase();

    const sortedMessages = Object.values(dbc.messages).sort((a, b) => a.name.localeCompare(b.name));

    const buildSignalRow = (sig: DbcSignal): HTMLElement => {
        const row = document.createElement("div");
        row.className = "signal-row";
        row.dataset.signal = sig.name;
        row.dataset.messageId = String(sig.message_id);
        row.dataset.channel = String(selectedChannel!);
        const key = plotKey(selectedChannel!, sig.message_id, sig.name);
        if (sig.enum_values?.length) signalEnums.set(key, sig.enum_values);
        signalUnits.set(key, sig.unit);
        // While paused, build from the pause-time snapshot so lazily expanded
        // messages match the frozen view.
        const snap = viewPaused ? sidebarSnapshot : null;
        const lastVal = (snap?.values ?? signalLastValues).get(key);
        const valText = lastVal != null ? sidebarValueText(key, lastVal, (snap?.raw ?? signalLastRaw).get(key), sig.unit) : (sig.unit || "");
        const mn = (snap?.min ?? signalMinValues).get(key);
        const mx = (snap?.max ?? signalMaxValues).get(key);
        const rangeText = mn !== undefined ? `↓${formatSigValue(mn, "")} ↑${formatSigValue(mx!, "")}` : "↓— ↑—";
        row.innerHTML = `
        <span class="sig-name">${sig.name}</span>
        <span class="sig-value${lastVal == null ? " sig-value--empty" : ""}">${valText}</span>
        <span class="sig-range${mn === undefined ? " sig-value--empty" : ""}">${rangeText}</span>`;
        signalValueEls.set(key, row.querySelector<HTMLElement>(".sig-value")!);
        signalRangeEls.set(key, row.querySelector<HTMLElement>(".sig-range")!);
        if (isKeyPlotted(key)) row.classList.add("in-plot");
        row.setAttribute("draggable", "true");
        // Drag / double-click behaviour is delegated on #dbc-tree (setupDbcTree).
        return row;
    };

    const buildMsgDetails = (msg: DbcMessage, container: HTMLElement) => {
        const noSignals = msg.signals.length === 0;
        const visibleSignals = msg.signals.filter(s =>
            !lc || s.name.toLowerCase().includes(lc) || msg.name.toLowerCase().includes(lc)
        );
        // A message with no signals has nothing to filter against but its own
        // name, so it stays visible whenever the filter matches (or is empty).
        if (noSignals ? (lc && !msg.name.toLowerCase().includes(lc)) : !visibleSignals.length) return;

        const details = document.createElement("details");
        details.className = "msg-group";
        details.dataset.channel = String(selectedChannel!);
        details.dataset.messageId = String(msg.id);

        const summary = document.createElement("summary");
        const emptyHint = noSignals ? `<span class="msg-empty-hint">(no signals)</span>` : "";
        summary.innerHTML = `${msg.name}${emptyHint}<span class="msg-id-badge">0x${msg.id.toString(16).toUpperCase().padStart(3, "0")}</span>`;
        // Drag / double-click behaviour is delegated on #dbc-tree (setupDbcTree).
        summary.setAttribute("draggable", "true");
        details.appendChild(summary);

        // Signal rows are built on first expand. A large DBC would otherwise keep
        // tens of thousands of rows (plus listeners) resident in the DOM.
        let populated = false;
        const populate = () => {
            if (populated) return;
            populated = true;
            for (const sig of visibleSignals) details.appendChild(buildSignalRow(sig));
        };
        details.addEventListener("toggle", () => { if (details.open) populate(); });
        if (filter) {
            details.open = true;
            populate();
        }
        container.appendChild(details);
    };

    // Group by ECU (transmitter). Messages without a named transmitter use "".
    const byEcu = new Map<string, DbcMessage[]>();
    for (const msg of sortedMessages) {
        const ecu = msg.transmitter ?? "";
        if (!byEcu.has(ecu)) byEcu.set(ecu, []);
        byEcu.get(ecu)!.push(msg);
    }
    // Nodes declared in the DBC but never used as a transmitter still get a
    // (message-less) group, so the tree reflects the full BU_ node list.
    for (const node of dbc.nodes ?? []) {
        if (!byEcu.has(node)) byEcu.set(node, []);
    }

    const hasNamedEcus = [...byEcu.keys()].some(k => k !== "");

    if (hasNamedEcus) {
        // Named ECUs first (sorted), unnamed last.
        const named = [...byEcu.entries()].filter(([k]) => k !== "").sort(([a], [b]) => a.localeCompare(b));
        const unnamed = byEcu.get("") ?? [];

        for (const [ecu, msgs] of named) {
            const ecuDetails = document.createElement("details");
            ecuDetails.className = "ecu-group";
            ecuDetails.open = true;
            const ecuSummary = document.createElement("summary");
            ecuSummary.className = "ecu-summary";
            ecuSummary.textContent = ecu;
            ecuDetails.appendChild(ecuSummary);
            for (const msg of msgs) buildMsgDetails(msg, ecuDetails);
            // A genuinely message-less ECU (not just filtered out) stays
            // visible with a hint, but only outside an active search filter.
            if (ecuDetails.childElementCount > 1) {
                tree.appendChild(ecuDetails);
            } else if (msgs.length === 0 && !lc) {
                const hint = document.createElement("span");
                hint.className = "ecu-empty-hint";
                hint.textContent = "(no messages)";
                ecuSummary.appendChild(hint);
                tree.appendChild(ecuDetails);
            }
        }

        if (unnamed.length > 0) {
            const ecuDetails = document.createElement("details");
            ecuDetails.className = "ecu-group";
            ecuDetails.open = true;
            const ecuSummary = document.createElement("summary");
            ecuSummary.className = "ecu-summary";
            ecuSummary.textContent = "Other";
            ecuDetails.appendChild(ecuSummary);
            for (const msg of unnamed) buildMsgDetails(msg, ecuDetails);
            if (ecuDetails.childElementCount > 1) tree.appendChild(ecuDetails);
        }
    } else {
        for (const msg of sortedMessages) buildMsgDetails(msg, tree);
    }

    // Refresh highlights after tree rebuild
    updateSignalHighlights();
}

// ── Drag & drop ───────────────────────────────────────────────────────────────

interface DragSignal {
    channel: number;
    signalName: string;
    messageName: string;
    unit: string;
    sig: DbcSignal;
}

interface DragMessage {
    channel: number;
    messageId: number;
    messageName: string;
    msg: DbcMessage;
}

function parseDragSignal(e: DragEvent): DragSignal | null {
    try { return JSON.parse(e.dataTransfer?.getData("application/can-signal") ?? "null"); }
    catch { return null; }
}

function parseDragMessage(e: DragEvent): DragMessage | null {
    try { return JSON.parse(e.dataTransfer?.getData("application/can-message") ?? "null"); }
    catch { return null; }
}

// Resolve a sidebar signal row back to its DBC definition. Rows only carry
// identifiers; keeping a closure per row would defeat the lazy tree.
function sigFromRow(row: HTMLElement): { handle: number; sig: DbcSignal } | null {
    const handle = parseInt(row.dataset.channel ?? "");
    const msgId = parseInt(row.dataset.messageId ?? "");
    const sig = channels.get(handle)?.dbc?.messages[msgId]?.signals.find(s => s.name === row.dataset.signal);
    return sig ? { handle, sig } : null;
}

// Resolve a sidebar message summary back to its DBC definition. The
// containing <details class="msg-group"> carries the identifiers.
function msgFromSummary(summary: HTMLElement): { handle: number; msg: DbcMessage } | null {
    const details = summary.closest<HTMLElement>(".msg-group");
    if (!details) return null;
    const handle = parseInt(details.dataset.channel ?? "");
    const msgId = parseInt(details.dataset.messageId ?? "");
    const msg = channels.get(handle)?.dbc?.messages[msgId];
    return msg ? { handle, msg } : null;
}

// One delegated dragstart/dblclick pair for the whole tree instead of two
// listeners per signal row.
function setupDbcTree() {
    const tree = document.getElementById("dbc-tree")!;
    tree.addEventListener("dragstart", (e) => {
        const target = e.target as HTMLElement;
        const row = target.closest<HTMLElement>(".signal-row");
        if (row) {
            const found = sigFromRow(row);
            if (!found) { e.preventDefault(); return; }
            const payload: DragSignal = {
                channel: found.handle,
                signalName: found.sig.name,
                messageName: found.sig.message_name,
                unit: found.sig.unit,
                sig: found.sig,
            };
            e.dataTransfer!.setData("application/can-signal", JSON.stringify(payload));
            e.dataTransfer!.effectAllowed = "copy";
            return;
        }
        const summary = target.closest<HTMLElement>(".msg-group > summary");
        if (summary) {
            const found = msgFromSummary(summary);
            if (!found) { e.preventDefault(); return; }
            const payload: DragMessage = {
                channel: found.handle,
                messageId: found.msg.id,
                messageName: found.msg.name,
                msg: found.msg,
            };
            e.dataTransfer!.setData("application/can-message", JSON.stringify(payload));
            e.dataTransfer!.effectAllowed = "copy";
        }
    });
    tree.addEventListener("dblclick", (e) => {
        const target = e.target as HTMLElement;
        const row = target.closest<HTMLElement>(".signal-row");
        if (row) {
            const found = sigFromRow(row);
            if (!found) return;
            const activeTab = document.querySelector(".tab-btn.active")?.getAttribute("data-tab");
            if (activeTab === "plot") {
                const pane = plotPanes[0] ?? createPlotPane();
                addSignalToPane(pane, found.handle, found.sig);
            } else if (activeTab === "simulate") {
                addSimSignal(found.handle, found.sig);
            }
            return;
        }
        const summary = target.closest<HTMLElement>(".msg-group > summary");
        if (summary) {
            const found = msgFromSummary(summary);
            if (!found) return;
            const activeTab = document.querySelector(".tab-btn.active")?.getAttribute("data-tab");
            if (activeTab === "plot") {
                const pane = plotPanes[0] ?? createPlotPane();
                addMessageSignalsToPane(pane, found.handle, found.msg);
            } else if (activeTab === "simulate") {
                addSimMessage(found.handle, found.msg);
            }
        }
    });
}

// Both a lone signal and a whole message are draggable; drop targets accept either.
function isCanDragEvent(e: DragEvent): boolean {
    const types = e.dataTransfer?.types;
    return !!types && (types.includes("application/can-signal") || types.includes("application/can-message"));
}

function setupPaneDrop(el: HTMLElement, pane: PlotPane) {
    let dragDepth = 0;
    el.addEventListener("dragenter", (e) => {
        if (!isCanDragEvent(e)) return;
        e.preventDefault();
        if (++dragDepth === 1) el.classList.add("drag-over");
    });
    el.addEventListener("dragover", (e) => {
        if (!isCanDragEvent(e)) return;
        e.preventDefault();
        e.dataTransfer!.dropEffect = "copy";
    });
    el.addEventListener("dragleave", () => {
        if (--dragDepth === 0) el.classList.remove("drag-over");
    });
    el.addEventListener("drop", (e) => {
        e.preventDefault();
        dragDepth = 0;
        el.classList.remove("drag-over");
        const msgData = parseDragMessage(e);
        if (msgData) { addMessageSignalsToPane(pane, msgData.channel, msgData.msg); return; }
        const data = parseDragSignal(e);
        if (data) addSignalToPane(pane, data.channel, data.sig);
    });
}

function setupDropZone() {
    const zone = document.getElementById("drop-zone-new")!;
    let dragDepth = 0;
    zone.addEventListener("dragenter", (e) => {
        if (!isCanDragEvent(e)) return;
        e.preventDefault();
        if (++dragDepth === 1) zone.classList.add("drag-over");
    });
    zone.addEventListener("dragover", (e) => {
        if (!isCanDragEvent(e)) return;
        e.preventDefault();
        e.dataTransfer!.dropEffect = "copy";
    });
    zone.addEventListener("dragleave", () => {
        if (--dragDepth === 0) zone.classList.remove("drag-over");
    });
    zone.addEventListener("drop", (e) => {
        e.preventDefault();
        dragDepth = 0;
        zone.classList.remove("drag-over");
        const msgData = parseDragMessage(e);
        if (msgData) { addMessageToNewPane(msgData.channel, msgData.msg); return; }
        const data = parseDragSignal(e);
        if (!data) return;
        const pane = createPlotPane();
        addSignalToPane(pane, data.channel, data.sig);
    });
}

function setupSimDrop() {
    const zone = document.getElementById("drop-zone-sim")!;
    let dragDepth = 0;
    zone.addEventListener("dragenter", (e) => {
        if (!isCanDragEvent(e)) return;
        e.preventDefault();
        if (++dragDepth === 1) zone.classList.add("drag-over");
    });
    zone.addEventListener("dragover", (e) => {
        if (!isCanDragEvent(e)) return;
        e.preventDefault();
        e.dataTransfer!.dropEffect = "copy";
    });
    zone.addEventListener("dragleave", () => {
        if (--dragDepth === 0) zone.classList.remove("drag-over");
    });
    zone.addEventListener("drop", (e) => {
        e.preventDefault();
        dragDepth = 0;
        zone.classList.remove("drag-over");
        const msgData = parseDragMessage(e);
        if (msgData) { addSimMessage(msgData.channel, msgData.msg); return; }
        const data = parseDragSignal(e);
        if (data) addSimSignal(data.channel, data.sig);
    });
}

// ── App state ─────────────────────────────────────────────────────────────────

// Single source of truth: u32 handle → everything we track about that channel.
const channels = new Map<number, Channel>();
// Available hardware interfaces populated when the "Add Channel" dialog opens.
let availableIfaces: ChannelInfo[] = [];

// Operating name for a channel: the custom display name when set, else the
// hardware interface name.
function channelName(handle: number): string {
    const ch = channels.get(handle);
    return ch ? (ch.config.display_name || ch.info.name) : String(handle);
}
const signalLastValues = new Map<string, number>();
const signalLastRaw = new Map<string, number>();
const signalMinValues = new Map<string, number>();
const signalMaxValues = new Map<string, number>();
const signalValueEls = new Map<string, HTMLElement>();
const signalRangeEls = new Map<string, HTMLElement>();
// Enum (VAL_) tables for signals currently shown in the sidebar tree, keyed by
// plotKey. Populated when the tree rows are built; used to append the named value.
const signalEnums = new Map<string, SignalEnumValue[]>();
// Unit per sidebar-visible signal (plotKey → unit); frame events no longer carry it.
const signalUnits = new Map<string, string>();
// Copies of the signal value maps taken when the view is paused. Signal rows
// built while paused (the tree populates lazily on expand) read from these so
// they show pause-time values; live tracking continues in the maps above.
interface SidebarSnapshot {
    values: Map<string, number>;
    raw: Map<string, number>;
    min: Map<string, number>;
    max: Map<string, number>;
}
let sidebarSnapshot: SidebarSnapshot | null = null;
let selectedChannel: number | null = null;

// Sidebar value text: physical value plus its DBC named value when the raw value
// maps to one (e.g. "3 (Third)").
function sidebarValueText(key: string, value: number, raw: number | undefined, unit: string): string {
    const base = formatSigValue(value, unit);
    const enums = signalEnums.get(key);
    if (enums && raw !== undefined) {
        const label = enums.find(e => e.value === raw)?.description;
        if (label) return `${base} (${label})`;
    }
    return base;
}

// ── Auto-save / session restore ───────────────────────────────────────────────

let sessionFilePath: string | null = null;
let autoSaveTimer: ReturnType<typeof setTimeout> | null = null;

let projectDirty = false;
function scheduleAutoSave(reason: string) {
    if (import.meta.env.DEV) {
        // stack[0] = "Error", [1] = scheduleAutoSave itself, [2] = the caller.
        const caller = new Error().stack?.split("\n")[2]?.trim().replace(/^at\s+/, "") ?? "unknown";
        log("debug", `Autosave scheduled: ${reason} (${caller})`);
    }
    if (!sessionFilePath) return;
    const project = buildProject();

    // Derive dirty state from an actual content comparison against the saved
    // project file, rather than assuming every edit leaves the project
    // unsaved — e.g. an edit that's undone back to the saved state shouldn't
    // keep the title's dirty marker lit.
    if (projectPath) {
        invoke<boolean>("project_has_changes", { path: projectPath, project })
            .then(changed => { projectDirty = changed; updateWindowTitle(); })
            .catch(e => log("debug", `Dirty check failed: ${e}`));
    } else {
        projectDirty = true;
        updateWindowTitle();
    }

    if (autoSaveTimer) clearTimeout(autoSaveTimer);
    autoSaveTimer = setTimeout(async () => {
        try {
            await invoke("save_project", { path: sessionFilePath, project });
        }
        // warn, not error: don't light up the error badge for a transient miss,
        // but the user should still see their session isn't being persisted.
        catch (e) { log("warn", `Session auto-save failed: ${e}`); }
    }, 1000);
}
let projectPath: string | null = null;
let lastProjectIndexPath: string | null = null;

function persistLastProjectPath(path: string) {
    if (lastProjectIndexPath)
        invoke("write_text_file", { path: lastProjectIndexPath, content: path })
            .catch(e => log("debug", `Failed to record last project path: ${e}`));
}

// ── Channel dialog ────────────────────────────────────────────────────────────

type DialogMode = "add" | "edit";
let dialogMode: DialogMode = "add";
let dialogEditTarget: number | null = null;
let dialogPendingDbc: string | null = null;

function setDbcLabel(path: string | null) {
    const lbl = document.getElementById("lbl-dbc-path")!;
    lbl.textContent = path ? path.split("/").pop()! : "None";
    lbl.title = path ?? "";
}

function getBitrateFromDialog(): number | null {
    const sel = (document.getElementById("select-bitrate") as HTMLSelectElement).value;
    if (sel === "vcan") return null;
    if (sel === "custom") return parseInt((document.getElementById("input-bitrate-custom") as HTMLInputElement).value) || null;
    return parseInt(sel);
}

function getProtocolFromDialog(): string | null {
    return (document.getElementById("select-protocol") as HTMLSelectElement).value || null;
}

function setProtocolInDialog(protocol: string | null) {
    (document.getElementById("select-protocol") as HTMLSelectElement).value = protocol ?? "";
}

function setBitrateInDialog(bitrate: number | null, isVcan: boolean) {
    const sel = document.getElementById("select-bitrate") as HTMLSelectElement;
    const custom = document.getElementById("input-bitrate-custom") as HTMLInputElement;
    if (isVcan) {
        sel.value = "vcan";
        custom.style.display = "none";
    } else if (bitrate != null && ["125000", "250000", "500000", "1000000"].includes(String(bitrate))) {
        sel.value = String(bitrate);
        custom.style.display = "none";
    } else if (bitrate != null) {
        sel.value = "custom";
        custom.value = String(bitrate);
        custom.style.display = "";
    } else {
        sel.value = "500000";
        custom.style.display = "none";
    }
}

async function openChannelDialog(mode: DialogMode, handle?: number) {
    dialogMode = mode;
    dialogEditTarget = handle ?? null;

    const dialog = document.getElementById("dialog-channel") as HTMLDialogElement;
    const title = document.getElementById("dialog-channel-title")!;
    const applyBtn = document.getElementById("btn-channel-apply")!;
    const ifaceRow = document.getElementById("row-iface")!;
    const sel = document.getElementById("select-iface") as HTMLSelectElement;

    const nameInput = document.getElementById("input-channel-name") as HTMLInputElement;

    if (mode === "add") {
        title.textContent = "Add CAN Channel";
        applyBtn.textContent = "Add";
        ifaceRow.style.display = "";
        sel.innerHTML = "";
        dialogPendingDbc = null;
        setDbcLabel(null);
        setBitrateInDialog(500000, false);
        setProtocolInDialog(null);
        nameInput.value = "";
        nameInput.placeholder = "Optional — defaults to interface name";
        const ifaces = await invoke<ChannelInfo[]>("list_can_interfaces").catch(() => [] as ChannelInfo[]);
        availableIfaces = ifaces;

        // Filter out interfaces already configured.
        const configured = new Set([...channels.values()].map(c => `${c.info.backend}:${c.info.name}`));
        const available = ifaces.filter(i => !configured.has(`${i.backend}:${i.name}`));

        if (available.length === 0) {
            log("warn", ifaces.length > 0 ? "All detected interfaces are already added." : "No CAN interfaces found.");
            dialog.close();
            return;
        }

        // Group remaining interfaces by backend into <optgroup> elements
        const byBackend = new Map<string, string[]>();
        for (const i of available) {
            const group = byBackend.get(i.backend) ?? [];
            group.push(i.name);
            byBackend.set(i.backend, group);
        }
        sel.innerHTML = [...byBackend.entries()]
            .map(([backend, names]) =>
                `<optgroup label="${escapeHtml(backend)}">${names.map(n => `<option value="${escapeHtml(n)}">${escapeHtml(n)}</option>`).join("")}</optgroup>`
            ).join("");

        // Auto-detect vcan from first available item
        if (available[0]?.name.startsWith("vcan")) setBitrateInDialog(null, true);
    } else {
        const ch = channels.get(handle!);
        const hwName = ch?.info.name ?? String(handle!);
        title.textContent = `Channel: ${channelName(handle!)}`;
        applyBtn.textContent = "Apply";
        ifaceRow.style.display = "none";
        dialogPendingDbc = ch?.config.dbc_path ?? null;
        setDbcLabel(dialogPendingDbc);
        setBitrateInDialog(ch?.config.bitrate ?? null, hwName.startsWith("vcan"));
        setProtocolInDialog(ch?.config.protocol ?? null);
        nameInput.value = ch?.config.display_name ?? "";
        nameInput.placeholder = hwName;
        selectChannel(handle!);
    }

    (document.activeElement as HTMLElement)?.blur();
    dialog.showModal();
}

// ── Sudo password ─────────────────────────────────────────────────────────────

function promptSudoPassword(): Promise<string | null> {
    return new Promise((resolve) => {
        const dialog = document.getElementById("dialog-sudo") as HTMLDialogElement;
        const input = document.getElementById("input-sudo-pw") as HTMLInputElement;
        const form = document.getElementById("form-sudo")!;
        const cancel = document.getElementById("btn-sudo-cancel")!;

        input.value = "";
        (document.activeElement as HTMLElement)?.blur();
        dialog.showModal();
        // Delay focus so the dialog is visible first
        setTimeout(() => input.focus(), 50);

        const done = (pw: string | null) => {
            input.value = "";
            form.removeEventListener("submit", onSubmit);
            cancel.removeEventListener("click", onCancel);
            dialog.close();
            resolve(pw);
        };
        const onSubmit = (e: Event) => { e.preventDefault(); done(input.value || null); };
        const onCancel = () => done(null);

        form.addEventListener("submit", onSubmit);
        cancel.addEventListener("click", onCancel);
    });
}

// Parse the channel's configured DBC into its `dbc` field so the signal tree is
// available before the channel is opened. No-op (clears dbc) when no DBC is set.
async function loadChannelDbc(handle: number): Promise<void> {
    const ch = channels.get(handle);
    if (!ch) return;
    sigKeyCache.delete(handle); // key tables are derived from the DBC being replaced
    pgnMapCache.delete(handle);
    if (!ch.config.dbc_path) { ch.dbc = null; return; }
    try {
        ch.dbc = await invoke<ParsedDbc>("parse_dbc", { path: ch.config.dbc_path });
    } catch (e) {
        ch.dbc = null;
        log("error", `Failed to parse DBC: ${e}`);
    }
}

// Result of registerChannel: `handle` is set on success. On failure `error`
// says why, and `notFound` marks the "exists in no backend" case that should
// become a ghost; other failures (duplicate, backend error) should not.
interface RegisterResult { handle?: number; error?: string; notFound?: boolean; }

// Register `config`'s channel with the backend and store it in `channels`.
// Owns everything every create-site needs to agree on: the invoke, writing the
// resolved backend back into config (create_channel searches all backends, so
// it can differ from the hint), the Channel shape, and the DBC preload. The
// same config object is stored on the channel, so ghost promotion keeps its
// saved settings.
async function registerChannel(config: ChannelConfig): Promise<RegisterResult> {
    let created: CreatedChannel;
    try {
        created = await invoke<CreatedChannel>("create_channel", { backendName: config.backend, channelName: config.name });
    } catch (e) {
        const error = String(e);
        return { error, notFound: error.includes("not found in any backend") };
    }
    // create_channel returns the existing handle when the name resolves to an
    // already-registered channel; overwriting it would clobber its config.
    const existing = channels.get(created.handle);
    if (existing) {
        return { error: `'${config.name}' resolves to already-configured channel ${existing.info.backend}:${existing.info.name}` };
    }
    config.backend = created.backend;
    channels.set(created.handle, { info: { backend: created.backend, name: config.name }, config, dbc: null, open: false });
    // Push the custom name to the backend so CSV exports use it too.
    if (config.display_name) {
        await invoke("set_channel_display_name", { channelHandle: created.handle, displayName: config.display_name })
            .catch(e => log("error", `Failed to set channel name: ${e}`));
    }
    await loadChannelDbc(created.handle);
    return { handle: created.handle };
}

// Open a channel by its u32 handle. If root is required the Rust side emits
// "request-admin-password", the global listener shows the dialog, and open_channel
// unblocks automatically. `quiet` demotes failure logs to debug — used by the
// automatic recovery retries so a persistently absent device doesn't spam the log.
async function openChannelByHandle(handle: number, quiet = false): Promise<boolean> {
    const ch = channels.get(handle);
    if (!ch) return false;
    try {
        // open_channel opens the hardware (using the channel's configured bitrate
        // and DBC path) and returns the DBC it parsed fresh from disk.
        const dbc = await invoke<ParsedDbc | null>("open_channel", {
            channelHandle: handle,
            bitrate: ch.config.bitrate ?? 500000,
            dbcPath: ch.config.dbc_path ?? null,
            protocol: ch.config.protocol ?? null,
        });
        ch.dbc = dbc ?? null;
        sigKeyCache.delete(handle); // key tables are derived from the DBC just replaced
        pgnMapCache.delete(handle);
        ch.open = true;
        ch.error = null;
        return true;
    } catch (e) {
        const msg = String(e);
        if (msg === "Sudo authentication cancelled") {
            log("warn", "Cancelled — sudo password required.");
        } else {
            log(quiet ? "debug" : "error", `Channel error: ${msg}`);
            if (!quiet) { ch.error = msg; renderChannelList(); }
        }
        return false;
    }
}

// ── Channel error handling / recovery ─────────────────────────────────────────
// A fatal "channel-error" means the channel's RX loop died (device unplugged,
// driver failure, bus-off). Close the dead half-open channel, badge it in the
// channel list, and — while capture is running — retry reopening in the
// background so replugging the device recovers without a manual restart.

const channelRecoveryTimers = new Map<number, ReturnType<typeof setTimeout>>();
const CHANNEL_RECOVERY_INTERVAL_MS = 5000;

async function onChannelError(ev: ChannelErrorEvent) {
    const ch = channels.get(ev.channel_handle);
    if (!ch) return;
    const name = channelName(ev.channel_handle);
    if (!ev.fatal) {
        // TX failures are already deduped per distinct error in the backend.
        log("warn", `TX error on ${name}: ${ev.error}`);
        return;
    }
    log("error", `Channel ${name} stopped receiving: ${ev.error}`);
    ch.error = ev.error;
    // The RX thread is gone but the hardware handle is still registered as
    // open; close it so the next open starts from a clean state.
    try { await invoke("close_channel", { channelHandle: ev.channel_handle }); }
    catch (e) { log("debug", `Close after channel error failed: ${e}`); }
    ch.open = false;
    renderChannelList();
    if (appRunning) scheduleChannelRecovery(ev.channel_handle);
}

function scheduleChannelRecovery(handle: number) {
    if (channelRecoveryTimers.has(handle)) return;
    channelRecoveryTimers.set(handle, setTimeout(async () => {
        channelRecoveryTimers.delete(handle);
        const ch = channels.get(handle);
        // Stop retrying once capture stops, the channel was removed, or
        // something else (Start, Reload backends) already reopened it.
        if (!ch || !appRunning || ch.open) return;
        if (await openChannelByHandle(handle, true)) {
            log("info", `Channel ${channelName(handle)} recovered`);
            renderChannelList();
            // Backend periodics died with the hardware handle; re-register the
            // sim entries the user still has marked as running on this channel.
            for (const [key, entry] of simEntries) {
                if (entry.channel === handle && entry.running) await startSim(key);
            }
        } else {
            scheduleChannelRecovery(handle);
        }
    }, CHANNEL_RECOVERY_INTERVAL_MS));
}

async function applyChannelDialog() {
    const dialog = document.getElementById("dialog-channel") as HTMLDialogElement;
    const bitrate = getBitrateFromDialog();
    const protocol = getProtocolFromDialog();
    const customName = (document.getElementById("input-channel-name") as HTMLInputElement).value.trim() || null;

    if (dialogMode === "add") {
        const name = (document.getElementById("select-iface") as HTMLSelectElement).value;
        if (!name) {
            log("error", "Channel name not set");
            return;
        };
        if (ghostChannels.some(g => g.config.name === name)) {
            log("error", `Channel '${name}' is already added (hardware not available)`);
            return;
        }
        const backend = availableIfaces.find(i => i.name === name)?.backend ?? "socketcan";
        const config: ChannelConfig = { name, display_name: customName, backend, dbc_path: dialogPendingDbc, bitrate, protocol };
        // Register channel with backend (allocates handle); hardware opens (and
        // the DBC is loaded) on Start. If the name exists in no backend the
        // channel is still added, as a ghost — same as a project channel whose
        // hardware is absent — and recovers once the hardware shows up. Any
        // other failure (duplicate, backend error) is a real error.
        const res = await registerChannel(config);
        if (res.handle === undefined) {
            if (!res.notFound) {
                log("error", `Failed to add channel: ${res.error}`);
                return;
            }
            ghostChannels.push({ config, error: res.error! });
            refreshChannelList();
            rebuildTraceColumns(); // J1939 columns appear/disappear with the protocol
            log("warn", `Added channel: ${customName ?? name} (hardware not available)`);
            scheduleAutoSave("channel added (hardware unavailable)");
            dialog.close();
            return;
        }

        const handle = res.handle;
        refreshChannelList();
        rebuildTraceColumns(); // J1939 columns appear/disappear with the protocol
        if (selectedChannel === handle) renderDbcTree();
        log("info", `Added channel: ${customName ?? name}`);
        scheduleAutoSave("channel added");
    } else {
        const h = dialogEditTarget!;
        const ch = channels.get(h);

        // Update config in place and reparse the DBC so the signal tree reflects
        // the change immediately (a fresh copy is also loaded on the next open).
        if (ch) {
            ch.config.display_name = customName;
            ch.config.dbc_path = dialogPendingDbc;
            ch.config.bitrate = bitrate;
            ch.config.protocol = protocol;
            // Sync (or clear) the custom name backend-side so CSV exports match.
            await invoke("set_channel_display_name", { channelHandle: h, displayName: customName })
                .catch(e => log("error", `Failed to set channel name: ${e}`));
            await loadChannelDbc(h);
        }
        const name = ch ? channelName(h) : String(h);

        refreshChannelList();
        rebuildTraceColumns(); // J1939 columns appear/disappear with the protocol
        if (selectedChannel === h) renderDbcTree();
        log("info", `Updated channel: ${name}`);
        scheduleAutoSave("channel updated");
    }

    dialog.close();
}

// ── Context menu ──────────────────────────────────────────────────────────────

let ctxMenu: HTMLElement | null = null;

function showContextMenu(x: number, y: number, items: { label: string; danger?: boolean; action: () => void }[]) {
    if (ctxMenu) ctxMenu.remove();
    const menu = document.createElement("div");
    menu.className = "ctx-menu";
    for (const item of items) {
        const btn = document.createElement("button");
        btn.className = "ctx-menu-item" + (item.danger ? " danger" : "");
        btn.textContent = item.label;
        btn.addEventListener("click", () => { menu.remove(); ctxMenu = null; item.action(); });
        menu.appendChild(btn);
    }
    menu.style.left = `${x}px`;
    menu.style.top = `${y}px`;
    document.body.appendChild(menu);
    ctxMenu = menu;

    // Flip if overflowing viewport
    const rect = menu.getBoundingClientRect();
    if (rect.right > window.innerWidth) menu.style.left = `${x - rect.width}px`;
    if (rect.bottom > window.innerHeight) menu.style.top = `${y - rect.height}px`;
}

document.addEventListener("click", () => { if (ctxMenu) { ctxMenu.remove(); ctxMenu = null; } });
document.addEventListener("keydown", (e) => { if (e.key === "Escape" && ctxMenu) { ctxMenu.remove(); ctxMenu = null; } });
if (!(import.meta as any).env?.DEV) document.addEventListener("contextmenu", (e) => e.preventDefault());

document.addEventListener("mousemove", (e) => {
    if (!midPan) return;
    if (!(e.buttons & 4)) { midPan = null; return; }
    if (plotPanes.length === 0 || midPan.chartWidth <= 0) return;
    const range = midPan.startMax - midPan.startMin;
    const dataDelta = ((e.clientX - midPan.startX) / midPan.chartWidth) * range;
    const newMin = midPan.startMin - dataDelta;
    const newMax = midPan.startMax - dataDelta;
    for (const p of plotPanes) {
        const xs = (p.chart.options.scales as any)["x"];
        xs.min = newMin;
        xs.max = newMax;
        p.chart.update("none");
    }
});
document.addEventListener("mouseup", (e) => { if (e.button === 1) midPan = null; });

// ── Hover tooltip ─────────────────────────────────────────────────────────────
// Native `title` tooltips don't reliably re-appear when the pointer moves
// straight from one titled element to an adjacent one. This custom tooltip shows
// instantly for any element carrying a `data-tip` attribute and follows the cursor.
let tooltipEl: HTMLElement | null = null;
let tipTarget: HTMLElement | null = null;
function positionTooltip(x: number, y: number) {
    if (!tooltipEl) return;
    tooltipEl.style.left = `${x + 12}px`;
    tooltipEl.style.top = `${y + 16}px`;
    const r = tooltipEl.getBoundingClientRect();
    if (r.right > window.innerWidth) tooltipEl.style.left = `${x - r.width - 4}px`;
    if (r.bottom > window.innerHeight) tooltipEl.style.top = `${y - r.height - 4}px`;
}
document.addEventListener("mouseover", (e) => {
    const t = (e.target as HTMLElement).closest<HTMLElement>("[data-tip]");
    if (!t) return;
    tipTarget = t;
    if (!tooltipEl) {
        tooltipEl = document.createElement("div");
        tooltipEl.className = "app-tooltip";
        document.body.appendChild(tooltipEl);
    }
    tooltipEl.textContent = t.dataset.tip ?? "";
    tooltipEl.style.display = "block";
    positionTooltip(e.clientX, e.clientY);
});
document.addEventListener("mousemove", (e) => {
    if (tipTarget) positionTooltip(e.clientX, e.clientY);
});
document.addEventListener("mouseout", (e) => {
    if (!tipTarget) return;
    const related = e.relatedTarget as HTMLElement | null;
    // Keep showing while moving between descendants of the same tipped element;
    // hide once the pointer leaves it (and isn't entering another tipped element
    // — that case re-fires mouseover and swaps the text instantly).
    if (related && related.closest("[data-tip]") === tipTarget) return;
    tipTarget = null;
    if (tooltipEl) tooltipEl.style.display = "none";
});

function showFilterMenu(
    x: number, y: number,
    items: { label: string; key: string }[],
    active: Set<string> | null,
    onFilter: (active: Set<string> | null) => void,
    topContent?: HTMLElement,
) {
    if (ctxMenu) ctxMenu.remove();
    const menu = document.createElement("div");
    menu.className = "ctx-menu filter-menu";
    menu.addEventListener("click", e => e.stopPropagation());

    if (topContent) menu.appendChild(topContent);

    const controls = document.createElement("div");
    controls.className = "filter-controls";
    const allBtn = document.createElement("button");
    allBtn.textContent = "All"; allBtn.className = "filter-ctrl-btn";
    const noneBtn = document.createElement("button");
    noneBtn.textContent = "None"; noneBtn.className = "filter-ctrl-btn";
    controls.append(allBtn, noneBtn);
    menu.appendChild(controls);

    const checkboxes: { el: HTMLInputElement; key: string }[] = [];
    for (const item of items) {
        const lbl = document.createElement("label");
        lbl.className = "filter-row";
        const cb = document.createElement("input");
        cb.type = "checkbox";
        cb.checked = active === null || active.has(item.key);
        lbl.append(cb, document.createTextNode(" " + item.label));
        menu.appendChild(lbl);
        checkboxes.push({ el: cb, key: item.key });
    }

    function notifyChange() {
        const checked = new Set(checkboxes.filter(c => c.el.checked).map(c => c.key));
        onFilter(checked.size === items.length ? null : checked);
    }

    for (const { el } of checkboxes) el.addEventListener("change", notifyChange);
    allBtn.addEventListener("click", (e) => { e.stopPropagation(); checkboxes.forEach(c => c.el.checked = true); onFilter(null); });
    noneBtn.addEventListener("click", (e) => { e.stopPropagation(); checkboxes.forEach(c => c.el.checked = false); onFilter(new Set()); });

    menu.style.left = `${x}px`;
    menu.style.top = `${y}px`;
    document.body.appendChild(menu);
    ctxMenu = menu;

    const rect = menu.getBoundingClientRect();
    if (rect.right > window.innerWidth) menu.style.left = `${x - rect.width}px`;
    if (rect.bottom > window.innerHeight) menu.style.top = `${y - rect.height}px`;
}

function showRangeFilterMenu(
    x: number, y: number,
    label: string,
    initMin: number | null, initMax: number | null,
    onChange: (min: number | null, max: number | null) => void,
) {
    if (ctxMenu) ctxMenu.remove();
    const menu = document.createElement("div");
    menu.className = "ctx-menu range-filter-menu";
    menu.addEventListener("click", ev => ev.stopPropagation());

    const state = { min: initMin, max: initMax };

    const title = document.createElement("div");
    title.className = "range-filter-title";
    title.textContent = label;
    menu.appendChild(title);

    for (const key of ["min", "max"] as const) {
        const row = document.createElement("div");
        row.className = "range-filter-row";
        const lbl = document.createElement("span");
        lbl.className = "range-filter-lbl";
        lbl.textContent = key === "min" ? "Min:" : "Max:";
        const inp = document.createElement("input");
        inp.type = "number"; inp.min = "0";
        inp.className = "range-filter-inp";
        inp.placeholder = "—";
        if (state[key] !== null) inp.value = String(state[key]);
        inp.addEventListener("input", () => {
            const v = parseFloat(inp.value);
            state[key] = inp.value.trim() && !isNaN(v) ? v : null;
            onChange(state.min, state.max);
        });
        row.append(lbl, inp);
        menu.appendChild(row);
    }

    const clearBtn = document.createElement("button");
    clearBtn.textContent = "Clear";
    clearBtn.className = "filter-ctrl-btn";
    clearBtn.style.cssText = "margin-top:6px;width:100%";
    clearBtn.addEventListener("click", () => { onChange(null, null); menu.remove(); ctxMenu = null; });
    menu.appendChild(clearBtn);

    menu.style.left = `${x}px`;
    menu.style.top = `${y}px`;
    document.body.appendChild(menu);
    ctxMenu = menu;
    const rect = menu.getBoundingClientRect();
    if (rect.right > window.innerWidth) menu.style.left = `${x - rect.width}px`;
    if (rect.bottom > window.innerHeight) menu.style.top = `${y - rect.height}px`;
}

// ── Channel management ────────────────────────────────────────────────────────

function selectChannel(handle: number | null) {
    selectedChannel = handle;
    renderChannelList();
    renderDbcTree((document.getElementById("signal-search") as HTMLInputElement).value);
}

function refreshChannelList() {
    const firstHandle = channels.size ? channels.keys().next().value as number : null;
    if (selectedChannel !== null && !channels.has(selectedChannel)) {
        selectChannel(firstHandle);
    } else if (selectedChannel === null && firstHandle !== null) {
        selectChannel(firstHandle);
    }
    renderChannelList();
    renderSimEntries();
}

async function renderChannelList() {
    const list = document.getElementById("channel-list")!;

    list.innerHTML = "";
    for (const [h, ch] of channels) {
        const dbcPath = ch.config.dbc_path;
        const bitrate = ch.config.bitrate ?? undefined;
        const name = ch.config.display_name || ch.info.name;
        const hwName = ch.info.name;
        const backend = ch.info.backend;
        const isSelected = h === selectedChannel;
        const bitrateLabel = hwName.startsWith("vcan") ? "vcan" : (bitrate ? `${(bitrate / 1000).toFixed(0)}k` : "—");
        const protoLabel = ch.config.protocol === "j1939" ? " · J1939" : "";
        const item = document.createElement("div");
        item.className = `channel-item${isSelected ? " selected" : ""}`;
        item.dataset.channelHandle = String(h);
        item.innerHTML = `
      <span class="dot${ch.open ? "" : ch.error ? " error" : " closed"}"${ch.error ? ` title="${escapeHtml(ch.error)}"` : ""}></span>
      <span class="ch-name" title="${escapeHtml(name === hwName ? name : `${name} (${hwName})`)}">${escapeHtml(name)}<span class="ch-backend label-muted"> ${backend}</span></span>
      <span class="ch-dbc"${dbcPath ? ` title="${dbcPath}"` : ""}>${dbcPath ? dbcPath.replace(/.*[/\\]/, "") : "No DBC"}</span>
      <span class="ch-baud label-muted">${bitrateLabel}${protoLabel}</span>
      <button class="btn-close-ch" title="Remove channel">×</button>
    `;
        item.addEventListener("click", (e) => {
            if ((e.target as HTMLElement).closest(".btn-close-ch")) return;
            selectChannel(h);
        });
        item.addEventListener("contextmenu", async (e) => {
            e.preventDefault();
            showContextMenu(e.clientX, e.clientY, [
                {
                    label: "Configure…", action: async () => {
                        if (!await confirmAndStop(`Stop live capture to configure "${name}"?`)) return;
                        openChannelDialog("edit", h);
                    }
                },
                {
                    label: "Remove Channel", danger: true, action: async () => {
                        if (!await confirmAndStop(`Stop live capture and remove "${name}"?`)) return;
                        channels.delete(h);
                        if (selectedChannel === h) selectChannel(null);
                        renderChannelList();
                        rebuildTraceColumns();
                        scheduleAutoSave("channel removed");
                    }
                },
            ]);
        });
        item.querySelector(".btn-close-ch")!.addEventListener("click", async (e) => {
            e.stopPropagation();
            if (!await confirmAndStop(`Stop live capture and remove "${name}"?`)) return;

            try { await invoke("remove_channel", { channelHandle: h }); }
            catch (e) {
                log("error", `Remove channel error: ${e}`);
                return;
            }
            channels.delete(h);

            if (selectedChannel === h) selectChannel(null);

            renderChannelList();
            rebuildTraceColumns();
            scheduleAutoSave("channel closed");
        });
        list.appendChild(item);
    }

    for (const ghost of ghostChannels) {
        const { config, error } = ghost;
        const dbcPath = config.dbc_path;
        const bitrate = config.bitrate;
        const ghostName = config.display_name || config.name;
        const bitrateLabel = config.name.startsWith("vcan") ? "vcan" : (bitrate ? `${(bitrate / 1000).toFixed(0)}k` : "—");
        const protoLabel = config.protocol === "j1939" ? " · J1939" : "";
        const item = document.createElement("div");
        item.className = "channel-item";
        item.innerHTML = `
      <span class="dot error" title="${error}"></span>
      <span class="ch-name" title="${escapeHtml(ghostName === config.name ? ghostName : `${ghostName} (${config.name})`)}">${escapeHtml(ghostName)}<span class="ch-backend label-muted"> ${config.backend}</span></span>
      <span class="ch-dbc"${dbcPath ? ` title="${dbcPath}"` : ""}>${dbcPath ? dbcPath.replace(/.*[/\\]/, "") : "No DBC"}</span>
      <span class="ch-baud label-muted">${bitrateLabel}${protoLabel}</span>
      <button class="btn-close-ch" title="Remove channel">×</button>
    `;
        item.querySelector(".btn-close-ch")!.addEventListener("click", (e) => {
            e.stopPropagation();
            ghostChannels.splice(ghostChannels.indexOf(ghost), 1);
            renderChannelList();
            rebuildTraceColumns();
            scheduleAutoSave("ghost channel removed");
        });
        list.appendChild(item);
    }
}

// ── Signal value helpers (physical ↔ raw) ─────────────────────────────────────

// Representable raw range for a signal given its bit length and signedness.
function signalRawRange(sig: DbcSignal): { min: number; max: number } {
    if (sig.signed && sig.length > 0) {
        const half = Math.pow(2, sig.length - 1);
        return { min: -half, max: half - 1 };
    }
    return { min: 0, max: Math.pow(2, sig.length) - 1 };
}

function physToRaw(sig: DbcSignal, phys: number): number {
    // A degenerate factor of 0 means every raw value encodes the same physical
    // (the offset) — use raw 0 rather than dividing to NaN.
    if (!sig.factor) return 0;
    // Float signals (SIG_VALTYPE_): the "raw" is the unscaled IEEE value — no
    // integer rounding.
    if (sig.float_bits) return (phys - sig.offset) / sig.factor;
    return Math.round((phys - sig.offset) / sig.factor);
}

function rawToPhys(sig: DbcSignal, raw: number): number {
    return raw * sig.factor + sig.offset;
}

// Clamp a physical value to the DBC-declared [min, max]. Many DBCs leave both at
// 0 to mean "unspecified" — only clamp when a real range is present.
function clampPhys(sig: DbcSignal, phys: number): number {
    if (sig.max > sig.min) return Math.min(sig.max, Math.max(sig.min, phys));
    return phys;
}

// Given a candidate physical value, return the physical + raw pair that will
// actually be sent. The raw value is the nearest integer the bit field can hold,
// and the physical value is snapped back to exactly what that raw encodes — so
// the box always shows the closest value that can really be transmitted. This is
// the single place value limits are enforced.
function normalizeSignalValue(sig: DbcSignal, phys: number): { phys: number; raw: number } {
    const clampedPhys = clampPhys(sig, phys);
    // Float signals carry any representable value — clamp to the DBC range but
    // never snap to an integer raw grid.
    if (sig.float_bits) return { phys: clampedPhys, raw: physToRaw(sig, clampedPhys) };
    const rr = signalRawRange(sig);
    const raw = Math.min(rr.max, Math.max(rr.min, physToRaw(sig, clampedPhys)));
    return { phys: rawToPhys(sig, raw), raw };
}

// Trim float noise for display without forcing a fixed precision.
function fmtNum(n: number): string {
    return String(Number(n.toFixed(6)));
}

// DBC named value (VAL_) label for a signal's raw integer value, or "" if none.
// VAL_ entries key on the raw value, so callers must pass the raw integer.
function enumLabelForRaw(sig: DbcSignal, raw: number): string {
    const enums = sig.enum_values ?? [];
    if (!enums.length) return "";
    return enums.find(e => e.value === raw)?.description ?? "";
}

// Physical signal-value map for a message entry, clamped to each signal's limits
// so a value entered (or restored from an old project) above max is never sent.
function simSignalValues(entry: SimMessageEntry): Record<string, number> {
    const out: Record<string, number> = {};
    for (const s of entry.signals) out[s.def.name] = normalizeSignalValue(s.def, s.value).phys;
    return out;
}

// ── Simulate tab ──────────────────────────────────────────────────────────────

interface SimMessageEntry {
    kind: "message";
    channel: number;
    messageId: number;
    messageName: string;
    dlc: number;
    signals: { def: DbcSignal; value: number }[];
    periodMs: number;
    running: boolean;
    periodicHandle: number | null;
}

interface SimRawEntry {
    kind: "raw";
    channel: number;
    canId: number;
    isExtended: boolean;
    dlc: number;
    data: number[];
    periodMs: number;
    running: boolean;
    periodicHandle: number | null;
}

type SimEntry = SimMessageEntry | SimRawEntry;

const simEntries = new Map<string, SimEntry>();
let rawEntryCounter = 0;
// Message sim entries are keyed by a unique instance id (not by message id) so the
// same message can be simulated multiple times concurrently.
let msgEntryCounter = 0;

// ── Sim entry element builders ────────────────────────────────────────────────

function createSimEntryEl(key: string, entry: SimEntry): HTMLElement {
    const el = document.createElement("div");
    el.className = "sim-group";
    el.dataset.simKey = key;

    if (entry.kind === "message") {
        const idHex = "0x" + entry.messageId.toString(16).toUpperCase().padStart(3, "0");
        // Reserve the enum column for the whole message (not per-row) so the raw
        // inputs stay aligned whether or not a given signal has named values.
        const hasEnums = entry.signals.some(s => (s.def.enum_values ?? []).length > 0);
        // Multiplexed message support: rows of a mux group not selected by the
        // switch's current value are dimmed — the backend won't encode them.
        const muxDef = entry.signals.find(s => s.def.multiplexor && s.def.mux_value == null)?.def
            ?? entry.signals.find(s => s.def.multiplexor)?.def;
        const currentMuxRaw = () => {
            const ms = muxDef && entry.signals.find(s => s.def === muxDef);
            return ms ? normalizeSignalValue(muxDef!, ms.value).raw : null;
        };
        const muxRaw0 = currentMuxRaw();
        el.innerHTML = `
      <div class="sim-group-header">
        <span class="sim-kind-badge kind-msg">MSG</span>
        <span class="sim-msg-name">${entry.messageName}</span>
        <span class="label-muted sim-msg-id">${idHex}</span>
        <span class="ch-badge">${escapeHtml(channelName(entry.channel))}</span>
        <span class="label-muted">Period</span>
        <input type="number" class="sim-period small-input" value="${entry.periodMs}" min="10">
        <span class="label-muted">ms</span>
        <div class="sim-actions">
          <button class="btn btn-sm sim-send-once">Send</button>
          <button class="btn btn-sm sim-toggle${entry.running ? " running" : ""}">${entry.running ? "Stop" : "Start"}</button>
          <button class="btn btn-sm btn-danger sim-remove">✕</button>
        </div>
      </div>
      <div class="sim-group-body${hasEnums ? " has-enums" : ""}">
        ${entry.signals.map((s, i) => {
            const { phys, raw } = normalizeSignalValue(s.def, s.value);
            const rr = signalRawRange(s.def);
            const isFloat = !!s.def.float_bits;
            // Arrow/spinner step = one raw unit's worth of physical value.
            // Float signals accept any value — no raw grid to step along.
            const physStep = !isFloat && Number.isFinite(s.def.factor) && s.def.factor !== 0 ? Math.abs(s.def.factor) : "any";
            const rangeTitle = s.def.max > s.def.min ? `Range: ${fmtNum(s.def.min)} … ${fmtNum(s.def.max)}` : "";
            const enums = s.def.enum_values ?? [];
            const enumSel = enums.length ? `
            <select class="sim-enum-sel" data-idx="${i}" title="Named values">
              <option value="" hidden disabled${enums.some(e => e.value === raw) ? "" : " selected"}>—</option>
              ${enums.map(e => `<option value="${e.value}"${e.value === raw ? " selected" : ""}>${e.description} (${e.value})</option>`).join("")}
            </select>` : "";
            const muxBadge = s.def.multiplexor
                ? `<span class="sim-mux-badge" title="Multiplexor switch">M</span>`
                : s.def.mux_value != null
                    ? `<span class="sim-mux-badge" title="Active when ${muxDef?.name ?? "switch"} = ${s.def.mux_value}">m${s.def.mux_value}</span>`
                    : "";
            const inactive = s.def.mux_value != null && s.def.mux_value !== muxRaw0;
            return `
          <div class="sim-signal-row${inactive ? " sim-sig-inactive" : ""}">
            <span class="sim-sig-name" title="${rangeTitle}">${s.def.name}${muxBadge}</span>
            <input type="number" class="sim-phys-input" data-idx="${i}" value="${fmtNum(phys)}" step="${physStep}"${rangeTitle ? ` min="${s.def.min}" max="${s.def.max}"` : ""} title="Physical value${isFloat ? "" : ` — step ${fmtNum(Math.abs(s.def.factor))}`}${s.def.unit ? " " + s.def.unit : ""}${rangeTitle ? " — " + rangeTitle : ""}">
            <span class="sim-sig-unit label-muted">${s.def.unit || ""}</span>
            <input type="number" class="sim-raw-input" data-idx="${i}" value="${isFloat ? fmtNum(raw) : raw}"${isFloat ? ` step="any" title="Unscaled IEEE float value"` : ` step="1" min="${rr.min}" max="${rr.max}" title="Raw value — range ${rr.min} … ${rr.max}"`}>
            <span class="sim-sig-raw-lbl label-muted">raw</span>
            ${enumSel}
          </div>`;
        }).join("")}
      </div>`;

        el.querySelector<HTMLInputElement>(".sim-period")!.addEventListener("input", async (e) => {
            const p = parseInt((e.target as HTMLInputElement).value) || 100;
            if (entry.running) { await stopSim(key); entry.periodMs = p; await startSim(key); }
            else entry.periodMs = p;
        });

        // Commit a new physical value for signal `i`: clamp, store, and refresh the
        // physical/raw/enum boxes so all three stay consistent. Restarts a running sim.
        const setSignalValue = async (i: number, candidatePhys: number) => {
            const s = entry.signals[i];
            const { phys, raw } = normalizeSignalValue(s.def, candidatePhys);
            s.value = phys;
            const row = el.querySelectorAll(".sim-signal-row")[i];
            const physInp = row.querySelector<HTMLInputElement>(".sim-phys-input")!;
            const rawInp = row.querySelector<HTMLInputElement>(".sim-raw-input")!;
            const enumSel = row.querySelector<HTMLSelectElement>(".sim-enum-sel");
            physInp.value = fmtNum(phys);
            rawInp.value = s.def.float_bits ? fmtNum(raw) : String(raw);
            if (enumSel) enumSel.value = (s.def.enum_values ?? []).some(e => e.value === raw) ? String(raw) : "";
            // Changing the multiplexor switch selects a different mux group;
            // re-dim the rows the backend will now skip when encoding.
            if (muxDef && s.def === muxDef) {
                const mr = currentMuxRaw();
                el.querySelectorAll(".sim-signal-row").forEach((r, j) => {
                    const mv = entry.signals[j]?.def.mux_value;
                    r.classList.toggle("sim-sig-inactive", mv != null && mv !== mr);
                });
            }
            if (entry.running) { await stopSim(key); await startSim(key); }
            scheduleAutoSave("sim signal value changed");
        };

        el.querySelectorAll<HTMLInputElement>(".sim-phys-input").forEach(inp => {
            inp.addEventListener("change", () => {
                setSignalValue(parseInt(inp.dataset.idx ?? "0"), parseFloat(inp.value) || 0);
            });
        });
        el.querySelectorAll<HTMLInputElement>(".sim-raw-input").forEach(inp => {
            inp.addEventListener("change", () => {
                const i = parseInt(inp.dataset.idx ?? "0");
                // parseFloat: float signals take fractional raw values; integer
                // signals get re-rounded by normalizeSignalValue anyway.
                const raw = parseFloat(inp.value) || 0;
                setSignalValue(i, rawToPhys(entry.signals[i].def, raw));
            });
        });
        el.querySelectorAll<HTMLSelectElement>(".sim-enum-sel").forEach(sel => {
            sel.addEventListener("change", () => {
                if (sel.value === "") return;
                const i = parseInt(sel.dataset.idx ?? "0");
                setSignalValue(i, rawToPhys(entry.signals[i].def, parseInt(sel.value)));
            });
        });
        el.querySelector(".sim-send-once")!.addEventListener("click", async () => {
            try { await invoke("send_message", { cmd: { channel_handle: entry.channel, message_id: entry.messageId, signal_values: simSignalValues(entry) } }); }
            catch (e) { log("error", `Send error: ${e}`); }
        });

    } else {
        const idHex = entry.canId.toString(16).toUpperCase().padStart(3, "0");
        el.innerHTML = `
      <div class="sim-group-header">
        <span class="sim-kind-badge kind-raw">RAW</span>
        <select class="sim-channel-sel">
          ${[...channels].map(([h, ch]) => `<option value="${h}"${h === entry.channel ? " selected" : ""}>${escapeHtml(ch.config.display_name || ch.info.name)}</option>`).join("")}
        </select>
        <span class="label-muted">Period</span>
        <input type="number" class="sim-period small-input" value="${entry.periodMs}" min="10">
        <span class="label-muted">ms</span>
        <div class="sim-actions">
          <button class="btn btn-sm sim-send-once">Send</button>
          <button class="btn btn-sm sim-toggle${entry.running ? " running" : ""}">${entry.running ? "Stop" : "Start"}</button>
          <button class="btn btn-sm btn-danger sim-remove">✕</button>
        </div>
      </div>
      <div class="sim-group-body">
        <div class="sim-raw-data-row">
          <span class="label-muted">ID</span>
          <input type="text" class="sim-canid-input small-input" value="${idHex}" maxlength="8" placeholder="hex">
          <label class="sim-ext-label label-muted"><input type="checkbox" class="sim-ext-cb"${entry.isExtended ? " checked" : ""}> Ext</label>
        </div>
        <div class="sim-raw-data-row">
          <span class="label-muted">DLC</span>
          <select class="sim-dlc-sel">
            ${[1, 2, 3, 4, 5, 6, 7, 8].map(n => `<option value="${n}"${n === entry.dlc ? " selected" : ""}>${n}</option>`).join("")}
          </select>
          <span class="label-muted">Data</span>
          <div class="sim-bytes">
            ${entry.data.map((b, i) => `<input type="text" class="sim-byte" data-idx="${i}" value="${b.toString(16).toUpperCase().padStart(2, "0")}" maxlength="2"${i >= entry.dlc ? " disabled" : ""}>`).join("")}
          </div>
        </div>
      </div>`;

        el.querySelector<HTMLSelectElement>(".sim-channel-sel")!.addEventListener("change", async (e) => {
            const wasRunning = entry.running;
            if (wasRunning) await stopSim(key);
            entry.channel = parseInt((e.target as HTMLSelectElement).value);
            if (wasRunning) await startSim(key);
        });
        el.querySelector<HTMLInputElement>(".sim-canid-input")!.addEventListener("input", async (e) => {
            const wasRunning = entry.running;
            if (wasRunning) await stopSim(key);
            entry.canId = parseInt((e.target as HTMLInputElement).value, 16) || 0;
            // An id that doesn't fit in 11 bits can only be an extended frame;
            // reflect that in the checkbox so the UI matches what will be sent.
            if (entry.canId > 0x7FF && !entry.isExtended) {
                entry.isExtended = true;
                el.querySelector<HTMLInputElement>(".sim-ext-cb")!.checked = true;
            }
            if (wasRunning) await startSim(key);
        });
        el.querySelector<HTMLInputElement>(".sim-ext-cb")!.addEventListener("change", async (e) => {
            const cb = e.target as HTMLInputElement;
            // Unchecking is only meaningful while the id fits in 11 bits.
            if (!cb.checked && entry.canId > 0x7FF) {
                cb.checked = true;
                return;
            }
            entry.isExtended = cb.checked;
            // Re-register a running periodic so the new frame format takes effect.
            if (entry.running) { await stopSim(key); await startSim(key); }
        });
        el.querySelector<HTMLSelectElement>(".sim-dlc-sel")!.addEventListener("change", async (e) => {
            entry.dlc = parseInt((e.target as HTMLSelectElement).value);
            el.querySelectorAll<HTMLInputElement>(".sim-byte").forEach((inp, i) => {
                inp.disabled = i >= entry.dlc;
            });
            if (entry.running) { await stopSim(key); await startSim(key); }
        });
        el.querySelector<HTMLInputElement>(".sim-period")!.addEventListener("input", async (e) => {
            const p = parseInt((e.target as HTMLInputElement).value) || 100;
            if (entry.running) { await stopSim(key); entry.periodMs = p; await startSim(key); }
            else entry.periodMs = p;
        });
        el.querySelectorAll<HTMLInputElement>(".sim-byte").forEach(inp => {
            inp.addEventListener("input", async () => {
                entry.data[parseInt(inp.dataset.idx ?? "0")] = parseInt(inp.value, 16) || 0;
                if (entry.running) { await stopSim(key); await startSim(key); }
            });
            inp.addEventListener("blur", () => {
                const i = parseInt(inp.dataset.idx ?? "0");
                inp.value = entry.data[i].toString(16).toUpperCase().padStart(2, "0");
            });
        });
        el.querySelector(".sim-send-once")!.addEventListener("click", async () => {
            try { await invoke("send_frame", { cmd: { channel_handle: entry.channel, can_id: entry.canId, data: entry.data.slice(0, entry.dlc), is_extended: entry.isExtended } }); }
            catch (e) { log("error", `Send error: ${e}`); }
        });
    }

    el.querySelector(".sim-toggle")!.addEventListener("click", async () => {
        entry.running ? await stopSim(key) : await startSim(key);
    });
    el.querySelector(".sim-remove")!.addEventListener("click", () => { removeSimEntry(key); });
    return el;
}

function renderSimEntries() {
    const container = document.getElementById("sim-entries")!;
    container.innerHTML = "";
    for (const [key, entry] of simEntries) {
        container.appendChild(createSimEntryEl(key, entry));
    }
}

// ── Sim actions ───────────────────────────────────────────────────────────────

function addSimSignal(handle: number, sig: DbcSignal) {
    const dbc = channels.get(handle)?.dbc;
    if (!dbc) { log("warn", "No DBC loaded for this channel"); return; }
    const msg = dbc.messages[sig.message_id];
    if (!msg) return;
    addSimMessage(handle, msg);
}

// A message is always simulated as a whole (one entry, all its signals);
// this also covers messages with zero signals — they get an entry with an
// empty signal list, sending a zero-filled frame of the message's DLC.
function addSimMessage(handle: number, msg: DbcMessage) {
    const key = `msg::${++msgEntryCounter}`;

    const entry: SimMessageEntry = {
        kind: "message", channel: handle,
        messageId: msg.id, messageName: msg.name, dlc: msg.dlc,
        signals: msg.signals.map(s => ({ def: s, value: s.min ?? 0 })),
        periodMs: 100, running: false, periodicHandle: null,
    };
    simEntries.set(key, entry);
    document.getElementById("sim-entries")!.appendChild(createSimEntryEl(key, entry));
    updateSignalHighlights();
    scheduleAutoSave("sim message added");
}

function addRawFrame() {
    const key = `raw::${++rawEntryCounter}`;
    const entry: SimRawEntry = {
        kind: "raw",
        channel: channels.size ? channels.keys().next().value as number : 0,
        canId: 0x100, isExtended: false, dlc: 8,
        data: new Array(8).fill(0),
        periodMs: 100, running: false, periodicHandle: null,
    };
    simEntries.set(key, entry);
    document.getElementById("sim-entries")!.appendChild(createSimEntryEl(key, entry));
    scheduleAutoSave("sim raw entry added");
}

async function removeSimEntry(key: string) {
    const entry = simEntries.get(key);
    if (entry?.running) await stopSim(key);
    simEntries.delete(key);
    document.querySelector(`[data-sim-key="${key}"]`)?.remove();
    updateSignalHighlights();
    scheduleAutoSave("sim entry removed");
}

async function startSim(key: string) {
    const entry = simEntries.get(key);
    // Guard: already sending (backend periodic registered).
    if (!entry || entry.periodicHandle !== null) return;
    if (!entry.channel) { log("warn", "Select a channel first"); return; }

    // Mark user intent immediately — button shows "Stop" even while app is stopped.
    entry.running = true;
    const btn = document.querySelector<HTMLButtonElement>(`[data-sim-key="${key}"] .sim-toggle`);
    if (btn) { btn.textContent = "Stop"; btn.classList.add("running"); }
    scheduleAutoSave("sim started");

    // Register with backend only when the app (and its channels) is live.
    if (!appRunning) return;

    try {
        let handle: number;
        if (entry.kind === "message") {
            handle = await invoke<number>("add_periodic_message", { cmd: { channel_handle: entry.channel, message_id: entry.messageId, signal_values: simSignalValues(entry), period_ms: entry.periodMs } });
        } else {
            handle = await invoke<number>("add_periodic_frame", { cmd: { channel_handle: entry.channel, can_id: entry.canId, data: entry.data.slice(0, entry.dlc), period_ms: entry.periodMs, is_extended: entry.isExtended } });
        }
        entry.periodicHandle = handle;
    } catch (e) {
        log("error", `Sim start error: ${e}`);
        entry.running = false;
        if (btn) { btn.textContent = "Start"; btn.classList.remove("running"); }
        scheduleAutoSave("sim start failed");
    }
}

async function stopSim(key: string) {
    const entry = simEntries.get(key);
    if (!entry || !entry.running) return;
    if (entry.periodicHandle !== null) {
        try { await invoke("remove_periodic", { cmd: { channel_handle: entry.channel, periodic_handle: entry.periodicHandle } }); } catch { }
        entry.periodicHandle = null;
    }
    entry.running = false;
    const btn = document.querySelector<HTMLButtonElement>(`[data-sim-key="${key}"] .sim-toggle`);
    if (btn) { btn.textContent = "Start"; btn.classList.remove("running"); }
    scheduleAutoSave("sim stopped");
}

// ── Project ───────────────────────────────────────────────────────────────────

// Channels are persisted by a stable "backend:name" id, since the u32 handle is
// ephemeral (reassigned each session). These convert at the persistence boundary.
function handleToId(handle: number): string {
    const ch = channels.get(handle);
    return ch ? `${ch.info.backend}:${ch.info.name}` : String(handle);
}

function idToHandle(id: string): number | undefined {
    for (const [h, ch] of channels) {
        if (`${ch.info.backend}:${ch.info.name}` === id) return h;
    }
    return undefined;
}

function buildProject(): Project {
    return {
        version: 1,
        channels: [
            ...[...channels.values()].map(ch => ({
                name: ch.info.name,
                display_name: ch.config.display_name ?? null,
                backend: ch.info.backend,
                dbc_path: ch.config.dbc_path,
                bitrate: ch.config.bitrate,
                protocol: ch.config.protocol,
            })),
            ...ghostChannels.map(g => ({
                name: g.config.name,
                display_name: g.config.display_name ?? null,
                backend: g.config.backend,
                dbc_path: g.config.dbc_path,
                bitrate: g.config.bitrate,
                protocol: g.config.protocol,
            })),
        ],
        plot_panes: plotPanes.map(pane => ({
            signals: [...pane.series.values()].map(s => ({ signal_name: s.signalName, channel: handleToId(s.channel), message_id: s.messageId })),
            interpolation: pane.interpolation,
            show_points: pane.showPoints,
            y_min: pane.yLock?.min ?? null,
            y_max: pane.yLock?.max ?? null,
        })),
        simulate_messages: [...simEntries.values()]
            .filter((e): e is SimMessageEntry => e.kind === "message")
            .map(e => ({
                channel: handleToId(e.channel),
                message_id: e.messageId,
                period_ms: e.periodMs,
                running: e.running,
                signals: e.signals.map(s => ({ name: s.def.name, value: s.value })),
            })),
        simulate_raw_frames: [...simEntries.values()]
            .filter((e): e is SimRawEntry => e.kind === "raw")
            .map(e => ({ channel: handleToId(e.channel), can_id: e.canId, is_extended: e.isExtended, dlc: e.dlc, data: e.data, period_ms: e.periodMs, running: e.running })),
        trace_filters: {
            channels: traceFilterChannels ? [...traceFilterChannels].map(handleToId) : null,
            can_ids: traceFilterCanIds ? [...traceFilterCanIds] : null,
            msg_names: traceFilterMsgNames ? [...traceFilterMsgNames] : null,
            dir: traceFilterDir ? [...traceFilterDir] : null,
            pgns: traceFilterPgns ? [...traceFilterPgns] : null,
            prios: traceFilterPrios ? [...traceFilterPrios] : null,
            sas: traceFilterSas ? [...traceFilterSas] : null,
            das: traceFilterDas ? [...traceFilterDas] : null,
            broadcast: traceFilterBroadcast,
            dlc_min: traceFilterDlcMin,
            dlc_max: traceFilterDlcMax,
            cycle_min: traceFilterCycleMin,
            cycle_max: traceFilterCycleMax,
            data: traceFilterData,
            data_format: traceDataFormat,
            max_rows: traceMaxRows,
        },
        window_size_sec: windowSizeSec,
        trace_columns: {
            order: traceColOrder,
            hidden: [...traceColHidden],
        },
    };
}

async function newProject() {
    if (projectDirty) {
        if (!await showConfirm("Discard unsaved changes and start a new project?")) return;
    }
    if (appRunning) await stopApp();

    for (const h of [...channels.keys()]) {
        try { await invoke("remove_channel", { channelHandle: h }); } catch { }
    }
    channels.clear();
    ghostChannels = [];

    while (plotPanes.length) closePlotPane(plotPanes[0].id);
    pendingPaneSignals = [];
    pendingSimMessages = [];

    for (const [key, entry] of simEntries) {
        if (entry.running) {
            try { await invoke("remove_periodic", { cmd: { channel_handle: entry.channel, periodic_handle: entry.periodicHandle } }); } catch { }
        }
        simEntries.delete(key);
    }
    document.getElementById("sim-entries")!.innerHTML = "";

    clearTrace();
    signalLastValues.clear();
    signalLastRaw.clear();
    signalMinValues.clear();
    signalMaxValues.clear();

    projectPath = null;
    projectDirty = false;
    sessionFilePath = null;
    updateWindowTitle();
    refreshChannelList();
    rebuildTraceColumns();
    renderDbcTree();
    log("info", "New project");
}

async function saveProject() {
    if (projectPath) {
        try {
            await invoke("save_project", { path: projectPath, project: buildProject() });
            projectDirty = false;
            updateWindowTitle();
            log("info", `Saved: ${projectPath}`);
        } catch (e) { log("error", `Save error: ${e}`); }
    } else { await saveProjectAs(); }
}

function ensureCanvazExt(path: string): string {
    return path.endsWith(".canvaz") ? path : `${path}.canvaz`;
}

async function saveProjectAs() {
    try {
        const raw = await dialogSave({ filters: [{ name: "Canvaz Project", extensions: ["canvaz"] }] });
        if (!raw) return;
        const path = ensureCanvazExt(raw);
        projectPath = path;
        projectDirty = false;
        updateWindowTitle();
        persistLastProjectPath(path);
        await invoke("save_project", { path, project: buildProject() });
        log("info", `Saved: ${path}`);
    } catch (e) { log("error", `Save error: ${e}`); }
}

async function openProject() {
    try {
        const path = await dialogOpen({ filters: [{ name: "Canvaz Project", extensions: ["canvaz"] }], multiple: false });
        if (!path || Array.isArray(path)) return;
        const project = await invoke<Project>("load_project", { path });
        projectPath = path;
        projectDirty = false;
        updateWindowTitle();
        persistLastProjectPath(path);
        await applyProject(project);
        log("info", `Loaded: ${path}`);
    } catch (e) { log("error", `Load error: ${e}`); }
}

async function applyProject(project: Project) {
    // Stop any active capture before applying a new project.
    if (appRunning) await stopApp();

    channels.clear();
    ghostChannels = [];

    for (const ch of project.channels) {
        const backend = ch.backend ?? "socketcan";
        const bitrate = ch.bitrate ?? null;
        const config: ChannelConfig = { name: ch.name, display_name: ch.display_name ?? null, backend, dbc_path: ch.dbc_path ?? null, bitrate, protocol: ch.protocol ?? null };
        // Any failure keeps the saved config as a ghost — including a name that
        // currently resolves to an already-added channel (a duplicate ghost
        // recovers under its own backend once that hardware appears, since
        // create_channel searches the hinted backend first).
        const res = await registerChannel(config);
        if (res.handle === undefined) {
            console.warn(`Channel ${ch.name} (${backend}) inactive: ${res.error}`);
            ghostChannels.push({ config, error: res.error! });
        }
    }

    refreshChannelList();
    renderDbcTree();
    rebuildTraceColumns(); // J1939 columns follow the loaded channels' protocols

    // Remove existing panes and create blank placeholders with correct settings.
    // Signals are added after startApp opens channels and the DBC is available.
    while (plotPanes.length) closePlotPane(plotPanes[0].id);
    pendingPaneSignals = [];

    for (const paneConfig of project.plot_panes) {
        const pane = createPlotPane();
        if (paneConfig.interpolation) {
            pane.interpolation = paneConfig.interpolation as PlotPane["interpolation"];
            pane.el.querySelector<HTMLSelectElement>(".sel-interp")!.value = pane.interpolation;
        }
        if (paneConfig.show_points) {
            pane.showPoints = true;
            const btn = pane.el.querySelector<HTMLButtonElement>(".btn-show-points")!;
            btn.classList.add("active");
            btn.title = "Show data points: on";
        }
        if (paneConfig.y_min != null && paneConfig.y_max != null) {
            setYLock(pane, { min: paneConfig.y_min, max: paneConfig.y_max }, false);
        }
        pendingPaneSignals.push(paneConfig.signals);
    }

    setWindowSize(project.window_size_sec ?? DEFAULT_WINDOW_SEC);

    simEntries.clear();
    document.getElementById("sim-entries")!.innerHTML = "";

    // Defer sim message restoration until startApp opens channels and loads DBCs.
    pendingSimMessages = project.simulate_messages ?? [];

    // Restore raw sim frames immediately (they don't need a DBC).
    // Preserve running intent: entry shows "Stop" if it was running when saved.
    for (const raw of project.simulate_raw_frames ?? []) {
        const key = `raw::${++rawEntryCounter}`;
        const rawHandle = idToHandle(raw.channel) ?? 0;
        const entry: SimRawEntry = {
            kind: "raw", channel: rawHandle,
            canId: raw.can_id, isExtended: raw.is_extended,
            dlc: raw.dlc, data: raw.data,
            periodMs: raw.period_ms, running: raw.running ?? false, periodicHandle: null,
        };
        simEntries.set(key, entry);
        document.getElementById("sim-entries")!.appendChild(createSimEntryEl(key, entry));
    }

    // Restore trace column layout
    if (project.trace_columns) {
        const tc = project.trace_columns;
        const validKeys = new Set(TRACE_COL_DEFS.map(d => d.key));
        if (tc.order?.length) {
            const saved = tc.order.filter(k => validKeys.has(k));
            const missing = TRACE_COL_DEFS.map(d => d.key).filter(k => !saved.includes(k));
            traceColOrder = [...saved, ...missing];
        }
        if (tc.hidden) traceColHidden = new Set(tc.hidden.filter(k => validKeys.has(k)));
        rebuildTraceColumns();
    }

    // Restore trace filter settings
    if (project.trace_filters) restoreTraceFilters(project.trace_filters);
}

function syncFilteredHeaders() {
    const th = (k: string) => document.querySelector<HTMLElement>(`#trace-table thead th[data-col="${k}"]`);
    th("channel")?.classList.toggle("th-filtered", traceFilterChannels !== null);
    th("canId")?.classList.toggle("th-filtered", traceFilterCanIds !== null);
    th("msg")?.classList.toggle("th-filtered", traceFilterMsgNames !== null);
    th("dir")?.classList.toggle("th-filtered", traceFilterDir !== null);
    th("pgn")?.classList.toggle("th-filtered", traceFilterPgns !== null || traceFilterBroadcast !== null);
    th("prio")?.classList.toggle("th-filtered", traceFilterPrios !== null);
    th("sa")?.classList.toggle("th-filtered", traceFilterSas !== null);
    th("da")?.classList.toggle("th-filtered", traceFilterDas !== null);
    th("dlc")?.classList.toggle("th-filtered", traceFilterDlcMin !== null || traceFilterDlcMax !== null);
    th("cycle")?.classList.toggle("th-filtered", traceFilterCycleMin !== null || traceFilterCycleMax !== null);
    th("data")?.classList.toggle("th-filtered", traceFilterData.some(v => v !== null));
}

function restoreTraceFilters(f: TraceFiltersConfig) {
    // Channels are persisted as stable "backend:name" ids; map back to handles
    // (channels are already created by this point in applyProject).
    traceFilterChannels = f.channels
        ? new Set(f.channels.map(idToHandle).filter((h): h is number => h !== undefined))
        : null;
    traceFilterCanIds = f.can_ids ? new Set(f.can_ids) : null;
    traceFilterMsgNames = f.msg_names ? new Set(f.msg_names) : null;
    traceFilterDir = f.dir ? new Set(f.dir) : null;
    traceFilterPgns = f.pgns ? new Set(f.pgns) : null;
    traceFilterPrios = f.prios ? new Set(f.prios) : null;
    traceFilterSas = f.sas ? new Set(f.sas) : null;
    traceFilterDas = f.das ? new Set(f.das) : null;
    traceFilterBroadcast = f.broadcast ?? null;
    traceFilterDlcMin = f.dlc_min ?? null;
    traceFilterDlcMax = f.dlc_max ?? null;
    traceFilterCycleMin = f.cycle_min ?? null;
    traceFilterCycleMax = f.cycle_max ?? null;
    // The saved array's length is the "bytes to check" count (older projects
    // always saved 8).
    const savedData = f.data ?? [];
    const dataLen = Math.min(MAX_DATA_FILTER_BYTES, Math.max(1, savedData.length || 8));
    traceFilterData = Array.from({ length: dataLen }, (_, i) => savedData[i] ?? null);

    if (f.data_format === "hex" || f.data_format === "dec" || f.data_format === "ascii")
        traceDataFormat = f.data_format;

    if (f.max_rows != null) {
        traceMaxRows = f.max_rows;
        (document.getElementById("input-trace-max") as HTMLInputElement).value = String(traceMaxRows);
    }

    // Seed "seen" sets from saved filter values so the filter menus open correctly
    traceFilterChannels?.forEach(v => traceSeenChannels.add(v));
    traceFilterCanIds?.forEach(v => traceSeenCanIds.add(v));
    if (traceFilterMsgNames) {
        traceFilterMsgNames.forEach(v => { if (v === "") traceSeenNoMsg = true; else traceSeenMsgNames.add(v); });
    }
    const seedJ1939 = (filter: Set<number> | null, seen: Set<number>) =>
        filter?.forEach(v => { if (v === -1) traceSeenNoJ1939 = true; else seen.add(v); });
    seedJ1939(traceFilterPgns, traceSeenPgns);
    seedJ1939(traceFilterPrios, traceSeenPrios);
    seedJ1939(traceFilterSas, traceSeenSas);
    seedJ1939(traceFilterDas, traceSeenDas);

    syncFilteredHeaders();
    updateClearFiltersBtn();
}

// ── App recording start / stop ────────────────────────────────────────────────

async function startApp() {
    // Retry ghost channels (hardware may have been plugged in since they were
    // added). Successfully recovered ghosts are moved into `channels` so they
    // open normally below. create_channel searches every backend for the name,
    // so a project channel saved with a stale backend still recovers.
    for (const ghost of [...ghostChannels]) {
        const res = await registerChannel(ghost.config);
        if (res.handle !== undefined) {
            ghostChannels.splice(ghostChannels.indexOf(ghost), 1);
        } else {
            ghost.error = res.error!;
            log("error", `Channel ${ghost.config.name} (${ghost.config.backend}) not available: ${res.error}`);
        }
    }

    // Open all configured channels (hardware connects here, not when added).
    // Each open_channel call parses the DBC fresh from disk and returns it.
    for (const handle of channels.keys()) {
        await openChannelByHandle(handle);
    }
    renderChannelList();

    // Reconcile simulated message entries with the reloaded DBC. Entries hold a
    // snapshot of each signal's definition, so after the DBC changes on disk we
    // prune entries whose message is gone and, for survivors, swap in the fresh
    // signal defs (preserving prior values by name), update the DLC, and re-render
    // so added/removed signals and changed scaling take effect.
    {
        const simContainer = document.getElementById("sim-entries")!;
        for (const [key, entry] of [...simEntries]) {
            if (entry.kind !== "message") continue;
            const msg = channels.get(entry.channel)?.dbc?.messages[entry.messageId];
            if (!msg) {
                simEntries.delete(key);
                simContainer.querySelector(`[data-sim-key="${key}"]`)?.remove();
                continue;
            }
            const prevValues = new Map(entry.signals.map(s => [s.def.name, s.value]));
            entry.messageName = msg.name;
            entry.dlc = msg.dlc;
            entry.signals = msg.signals.map(s => ({ def: s, value: prevValues.get(s.name) ?? s.min ?? 0 }));
            simContainer.querySelector(`[data-sim-key="${key}"]`)?.replaceWith(createSimEntryEl(key, entry));
        }
    }

    // Prune message names from filter that no longer exist in any DBC
    if (traceFilterMsgNames !== null) {
        const validNames = new Set([...channels.values()].flatMap(m => m.dbc ? Object.values(m.dbc.messages).map(msg => msg.name) : []));
        for (const name of [...traceFilterMsgNames]) {
            if (name !== "" && !validNames.has(name)) traceFilterMsgNames.delete(name);
        }
        if (traceFilterMsgNames.size === 0) traceFilterMsgNames = null;
        syncFilteredHeaders();
    }

    appRunning = true;
    appStartTime = Date.now();
    signalLastValues.clear();
    signalLastRaw.clear();
    signalMinValues.clear();
    signalMaxValues.clear();

    // Rebuild DBC tree to clear sidebar values
    renderDbcTree((document.getElementById("signal-search") as HTMLInputElement).value);

    // Reset global pause state
    viewPaused = false;
    sidebarSnapshot = null;
    updatePauseViewBtn();

    // Clear all plot pane data, reset zoom and X-axis bounds. Cursors are
    // dropped too: their positions belong to the previous capture's timeline.
    for (const pane of plotPanes) {
        for (const s of pane.series.values()) { s.timestamps.length = 0; s.data.length = 0; s.lastValue = null; s.frozenData = null; }
        setPaneCursors(pane, false);
        clearPaneZoom(pane);
        const xScale = (pane.chart.options.scales as any)["x"];
        xScale.min = 0;
        xScale.max = windowSizeSec;
        pane.chart.update();
    }

    // Restore pane signals deferred from applyProject (DBC is now available from open_channel).
    if (pendingPaneSignals.length > 0) {
        const toRestore = pendingPaneSignals;
        pendingPaneSignals = [];
        for (let i = 0; i < Math.min(plotPanes.length, toRestore.length); i++) {
            for (const entry of toRestore[i]) {
                const handle = idToHandle(entry.channel);
                if (handle === undefined) continue;
                const dbc = channels.get(handle)?.dbc;
                const sig = dbc && Object.values(dbc.messages).flatMap((m: DbcMessage) => m.signals).find(
                    (s: DbcSignal) => entry.message_id !== undefined
                        ? s.message_id === entry.message_id && s.name === entry.signal_name
                        : s.name === entry.signal_name
                );
                if (sig) await addSignalToPane(plotPanes[i], handle, sig);
            }
        }
    }

    // Restore per-instance sim message entries (new format) deferred from applyProject.
    if (pendingSimMessages.length > 0) {
        const toRestore = pendingSimMessages;
        pendingSimMessages = [];
        const simContainer = document.getElementById("sim-entries")!;
        for (const m of toRestore) {
            const handle = idToHandle(m.channel);
            if (handle === undefined) continue;
            const msg = channels.get(handle)?.dbc?.messages[m.message_id];
            if (!msg) continue;
            const valueByName = new Map(m.signals.map(s => [s.name, s.value]));
            const key = `msg::${++msgEntryCounter}`;
            const simEntry: SimMessageEntry = {
                kind: "message", channel: handle,
                messageId: msg.id, messageName: msg.name, dlc: msg.dlc,
                signals: msg.signals.map(s => ({ def: s, value: valueByName.get(s.name) ?? s.min ?? 0 })),
                periodMs: m.period_ms, running: m.running ?? false, periodicHandle: null,
            };
            simEntries.set(key, simEntry);
            simContainer.appendChild(createSimEntryEl(key, simEntry));
        }
        renderSimEntries();
        // The DBC tree was rendered above before these entries existed; re-apply the
        // message-level simulation indicators now that simEntries is populated.
        updateSignalHighlights();
    }

    // Register backend periodics for all entries the user has marked as running
    // (covers both restored entries and entries that survived a stop/start cycle).
    for (const [key, entry] of simEntries) {
        if (entry.running) await startSim(key);
    }

    clearTrace();
    startScrollLoop();

    const btn = document.getElementById("btn-app-run")!;
    btn.textContent = "■ Stop";
    btn.classList.add("running");
    btn.title = "Pause live capture";
    log("info", "Live capture started");
}

async function stopApp() {
    // If the view is paused, resume it first so pending trace/plot/sidebar
    // updates are flushed and the final live state is shown before stopping.
    if (viewPaused) {
        viewPaused = false;
        resumeFromPause();
    }
    appRunning = false;
    updatePauseViewBtn();
    // Close hardware connections; they will reopen on the next Start.
    for (const [handle, ch] of channels) {
        // A deliberate stop clears error badges — everything is closed now and
        // the next Start reports fresh errors if the problem persists.
        ch.error = null;
        if (!ch.open) continue;
        try { await invoke("close_channel", { channelHandle: handle }); }
        catch (e) { log("debug", `Failed to close channel ${ch.info.name}: ${e}`); }
        ch.open = false;
    }
    // Channels are closed so backend periodics are gone; preserve running intent so
    // entries auto-restart on next Start and the UI keeps showing "Stop".
    for (const entry of simEntries.values()) {
        entry.periodicHandle = null;
    }
    renderChannelList();
    const btn = document.getElementById("btn-app-run")!;
    btn.textContent = "▶ Start";
    btn.classList.remove("running");
    btn.title = "Start live capture";
    log("info", "Stopped");
}

function showConfirm(message: string): Promise<boolean> {
    return new Promise((resolve) => {
        const dialog = document.getElementById("dialog-confirm") as HTMLDialogElement;
        document.getElementById("dialog-confirm-msg")!.textContent = message;

        const ok = document.getElementById("btn-confirm-ok")!;
        const cancel = document.getElementById("btn-confirm-cancel")!;

        const done = (result: boolean) => {
            ok.removeEventListener("click", onOk);
            cancel.removeEventListener("click", onCancel);
            dialog.removeEventListener("cancel", onCancel);
            dialog.close();
            resolve(result);
        };
        const onOk = () => done(true);
        const onCancel = () => done(false);

        ok.addEventListener("click", onOk);
        cancel.addEventListener("click", onCancel);
        dialog.addEventListener("cancel", onCancel); // Escape key
        (document.activeElement as HTMLElement)?.blur();
        dialog.showModal();
    });
}

// Shows a confirm dialog if running, stops capture, then returns true so the caller can proceed.
async function confirmAndStop(prompt: string): Promise<boolean> {
    if (!appRunning) return true;
    if (!await showConfirm(prompt)) return false;
    await stopApp();
    return true;
}

async function exportCsv() {
    const path = await dialogSave({
        defaultPath: "signals.csv",
        filters: [{ name: "CSV Files", extensions: ["csv"] }],
    });
    if (!path) return;
    try {
        const count = await invoke<number>("export_signals_csv", { path, startMs: appStartTime });
        log("info", count > 0 ? `Exported ${count} signal samples to CSV` : "No signal data to export");
    } catch (e) { log("error", `Export error: ${e}`); }
}

async function exportTraceCsv() {
    const path = await dialogSave({
        defaultPath: "trace.csv",
        filters: [{ name: "CSV Files", extensions: ["csv"] }],
    });
    if (!path) return;
    try {
        const count = await invoke<number>("export_frames_csv", { path, startMs: appStartTime });
        log("info", count > 0 ? `Exported ${count} frames to CSV` : "No trace data to export");
    } catch (e) { log("error", `Export error: ${e}`); }
}

// ── Preferences ─────────────────────────────────────────────────────────────
// Per-user global preferences, persisted across restarts in the user's app data
// dir (next to the session file, not in the project file). The whole object is
// loaded into memory on startup and written back as a whole so new keys added
// in the future are preserved. Sidebar width is the first such preference.

interface Preferences {
    sidebarWidth?: number;
    // Latest release version the user chose to skip. While this matches the
    // newest release we won't prompt at startup; a newer release clears it.
    skippedVersion?: string;
    // Message log docked into the layout (and kept open) instead of floating.
    logPinned?: boolean;
}

let preferencesPath: string | null = null;
let preferences: Preferences = {};

async function loadPreferences() {
    try {
        const dir = await invoke<string>("get_app_data_dir");
        preferencesPath = `${dir}/preferences.json`;
        const raw = await invoke<string>("read_text_file", { path: preferencesPath });
        preferences = JSON.parse(raw);
    } catch {
        // No saved preferences yet — keep defaults.
    }
}

function savePreferences() {
    if (!preferencesPath) return;
    invoke("write_text_file", {
        path: preferencesPath,
        content: JSON.stringify(preferences, null, 2),
    }).catch(e => log("debug", `Failed to save preferences: ${e}`));
}

// ── Update checker ────────────────────────────────────────────────────────────
// Compares the running version against the newest published GitHub release. At
// startup we only nag on real release builds (clean semver); development builds
// are skipped automatically but can still check manually via About → Check for
// Updates. Skipping a version is remembered in preferences until a newer one
// appears.

const GITHUB_REPO = "rustypig91/canvaz";
const RELEASES_LATEST_API = `https://api.github.com/repos/${GITHUB_REPO}/releases/latest`;
const RELEASES_PAGE_URL = `https://github.com/${GITHUB_REPO}/releases/latest`;

interface LatestRelease {
    version: string; // Version tag e.g. "v0.1.0"
    url: string;     // page to open for the download
}

let currentVersion = "";

// The in-app version build.rs bakes in. Release builds are a clean "X.Y.Z";
// dev builds carry a git-describe suffix ("0.1.0-5-gabc", "-modified") or are
// "unknown". Only clean release builds prompt at startup.
function isReleaseBuild(version: string): boolean {
    return /^v\d+\.\d+\.\d+$/.test(version);
}

// True when `latest` is genuinely newer than what we run. A dev build of the
// same base version (e.g. "v0.1.0-5-gabc" vs released "v0.1.0") is not an update.
function isNewerVersion(current: string, latest: string): boolean {
    if (!latest) return false;
    if (current === latest) return false;
    if (current.startsWith(`${latest}-`)) return false;
    return true;
}

async function fetchLatestRelease(): Promise<LatestRelease> {
    const resp = await fetch(RELEASES_LATEST_API, {
        headers: { Accept: "application/vnd.github+json" },
    });
    if (!resp.ok) throw new Error(`GitHub API returned ${resp.status}`);
    const data = await resp.json() as { tag_name?: string; html_url?: string };
    return {
        version: data.tag_name ?? "",
        url: data.html_url || RELEASES_PAGE_URL,
    };
}

function openUpdateDialog(opts: {
    title: string;
    message: string;
    downloadUrl: string | null;
    skipVersion: string | null;
}) {
    document.getElementById("update-title")!.textContent = opts.title;
    document.getElementById("update-message")!.textContent = opts.message;

    const downloadBtn = document.getElementById("btn-update-download") as HTMLButtonElement;
    const skipBtn = document.getElementById("btn-update-skip") as HTMLButtonElement;
    const closeBtn = document.getElementById("btn-update-close") as HTMLButtonElement;
    const dialog = document.getElementById("dialog-update") as HTMLDialogElement;

    downloadBtn.style.display = opts.downloadUrl ? "" : "none";
    skipBtn.style.display = opts.skipVersion ? "" : "none";
    // With no update to download, the dialog is just an acknowledgement — label the
    // dismiss button "Ok" instead of "Later" (which implies a pending action).
    closeBtn.textContent = opts.downloadUrl ? "Later" : "Ok";

    downloadBtn.onclick = () => { if (opts.downloadUrl) openUrl(opts.downloadUrl); };
    skipBtn.onclick = () => {
        if (opts.skipVersion) { preferences.skippedVersion = opts.skipVersion; savePreferences(); }
        dialog.close();
    };

    (document.activeElement as HTMLElement)?.blur();
    dialog.showModal();
}

function openSysResDialog() {
    const dialog = document.getElementById("dialog-sysres") as HTMLDialogElement;
    const tbody = document.getElementById("sysres-body")!;
    const totalCpuEl = document.getElementById("sysres-total-cpu")!;
    const totalMemEl = document.getElementById("sysres-total-mem")!;
    const framesEl = document.getElementById("sysres-frames")!;

    type ProcessInfo = { name: string; pid: number; cpu: number; memory: number };
    type SystemResources = { processes: ProcessInfo[]; frame_count: number; frame_bytes: number };

    function mb(bytes: number) { return (bytes / 1024 / 1024).toFixed(1) + " MB"; }

    function refresh() {
        invoke<SystemResources>("system_resources")
            .then(({ processes, frame_count, frame_bytes }) => {
                tbody.innerHTML = processes.map(p =>
                    `<tr>
                        <td>${p.name}</td>
                        <td>${p.pid}</td>
                        <td>${p.cpu.toFixed(1)} %</td>
                        <td>${mb(p.memory)}</td>
                    </tr>`
                ).join("") || `<tr><td colspan="4" class="sysres-label">No processes found</td></tr>`;

                const totalCpu = processes.reduce((s, p) => s + p.cpu, 0);
                const totalMem = processes.reduce((s, p) => s + p.memory, 0);
                totalCpuEl.textContent = totalCpu.toFixed(1) + " %";
                totalMemEl.textContent = mb(totalMem);

                framesEl.textContent = `Frame buffer: ${frame_count.toLocaleString()} frames · ${mb(frame_bytes)}`;
            })
            .catch(() => {
                tbody.innerHTML = `<tr><td colspan="4" class="sysres-label">Error reading resources</td></tr>`;
            });
    }

    refresh();
    const timer = setInterval(refresh, 1000);
    dialog.addEventListener("close", () => clearInterval(timer), { once: true });
    (document.activeElement as HTMLElement)?.blur();
    dialog.showModal();
}

// `manual` distinguishes the explicit About → Check for Updates action (which
// always reports a result and ignores the skip preference) from the silent
// startup check (which only surfaces a brand-new, non-skipped release).
async function checkForUpdates(manual: boolean) {
    if (manual) log("info", "Checking for updates…");

    let latest: LatestRelease;
    try {
        latest = await fetchLatestRelease();
    } catch (e) {
        if (manual) openUpdateDialog({ title: "Update check failed", message: `Could not reach GitHub: ${e}`, downloadUrl: null, skipVersion: null });
        return;
    }

    if (!latest.version) {
        if (manual) openUpdateDialog({ title: "Update check failed", message: "Could not determine the latest version.", downloadUrl: null, skipVersion: null });
        return;
    }

    if (!isNewerVersion(currentVersion, latest.version)) {
        if (manual) openUpdateDialog({ title: "Up to date", message: `You're running the latest version (${currentVersion}).`, downloadUrl: null, skipVersion: null });
        return;
    }

    // A newer release exists. Honour a prior skip only for the silent check.
    if (!manual && preferences.skippedVersion === latest.version) return;

    openUpdateDialog({
        title: "Update available",
        message: `Version ${latest.version} is available — you're on ${currentVersion}.`,
        downloadUrl: latest.url,
        skipVersion: latest.version,
    });
}

// ── Window size (data retention) ─────────────────────────────────────────────
// Signal values older than the window are discarded, bounding memory usage.
// The window size is a per-project setting (stored in the project file).

const WINDOW_PRESETS = new Set([30, 60, 300, 900, 1800]);
const DEFAULT_WINDOW_SEC = 60;
let windowSizeSec = DEFAULT_WINDOW_SEC;

function pruneOldData() {
    // Prune even while the view is paused — the frozen display uses its own
    // snapshot, so the live arrays must not grow unbounded during long pauses.
    if (!appRunning) return; // leave a stopped chart untouched
    const cutoff = Date.now() - windowSizeSec * 1000;
    for (const pane of plotPanes) {
        for (const s of pane.series.values()) {
            let i = 0;
            while (i < s.timestamps.length && s.timestamps[i] < cutoff) i++;
            // Replace the last out-of-window point with an interpolated anchor at exactly
            // xScale.min so the line extends to the left edge and the Y-axis range includes it.
            if (i >= 1) {
                const p0 = s.data[i - 1];
                const p1 = s.data[i];
                if (p1 !== undefined && p0.x !== p1.x) {
                    const cutoffX = (cutoff - appStartTime) / 1000;
                    const ratio = (cutoffX - p0.x) / (p1.x - p0.x);
                    s.data[i - 1] = { x: cutoffX, y: p0.y + ratio * (p1.y - p0.y) };
                    s.timestamps[i - 1] = cutoff;
                }
                if (i > 1) { s.timestamps.splice(0, i - 1); s.data.splice(0, i - 1); }
            }
        }
        markPaneDirty(pane);
    }
}

// Sync the toolbar controls to the current windowSizeSec.
function reflectWindowSize() {
    const select = document.getElementById("select-window") as HTMLSelectElement;
    const custom = document.getElementById("input-window-custom") as HTMLInputElement;
    if (WINDOW_PRESETS.has(windowSizeSec)) {
        select.value = String(windowSizeSec);
        custom.style.display = "none";
    } else {
        select.value = "custom";
        custom.value = String(windowSizeSec);
        custom.style.display = "";
    }
}

// Apply a window size loaded from a project (updates UI + prunes, no autosave).
function setWindowSize(sec: number) {
    windowSizeSec = Math.max(1, Math.round(sec));
    reflectWindowSize();
    pruneOldData();
    invoke("set_window_ms", { ms: windowSizeSec * 1000 }).catch(() => { });
}

// User changed the control: apply and mark the project dirty.
function applyWindowSize(sec: number) {
    setWindowSize(sec);
    scheduleAutoSave("window size changed");
}

function setupWindowSize() {
    const select = document.getElementById("select-window") as HTMLSelectElement;
    const custom = document.getElementById("input-window-custom") as HTMLInputElement;

    reflectWindowSize();

    select.addEventListener("change", () => {
        if (select.value === "custom") {
            custom.style.display = "";
            custom.focus();
            const v = parseInt(custom.value);
            if (v > 0) applyWindowSize(v);
        } else {
            custom.style.display = "none";
            applyWindowSize(parseInt(select.value));
        }
    });
    custom.addEventListener("change", () => {
        const v = parseInt(custom.value);
        if (v > 0) applyWindowSize(v);
    });

    // Periodically discard data older than the window.
    setInterval(pruneOldData, 1000);
}

// ── Sidebar resize ─────────────────────────────────────────────────────────────

const SIDEBAR_MIN_WIDTH = 200;
const SIDEBAR_MAX_WIDTH = 600;

function applySidebarWidth() {
    if (typeof preferences.sidebarWidth === "number") {
        document.getElementById("sidebar")!.style.width = `${preferences.sidebarWidth}px`;
    }
}

function setupSidebarResize() {
    const sidebar = document.getElementById("sidebar")!;
    const resizer = document.getElementById("sidebar-resizer")!;
    let dragging = false;

    resizer.addEventListener("mousedown", (e) => {
        e.preventDefault();
        dragging = true;
        resizer.classList.add("dragging");
        document.body.classList.add("resizing");
    });

    window.addEventListener("mousemove", (e) => {
        if (!dragging) return;
        const left = sidebar.getBoundingClientRect().left;
        const width = Math.max(SIDEBAR_MIN_WIDTH, Math.min(SIDEBAR_MAX_WIDTH, e.clientX - left));
        sidebar.style.width = `${width}px`;
    });

    window.addEventListener("mouseup", () => {
        if (!dragging) return;
        dragging = false;
        resizer.classList.remove("dragging");
        document.body.classList.remove("resizing");
        preferences.sidebarWidth = Math.round(sidebar.getBoundingClientRect().width);
        savePreferences();
    });

    // Double-click resets the sidebar to its default (CSS) width.
    resizer.addEventListener("dblclick", () => {
        sidebar.style.width = "";
        delete preferences.sidebarWidth;
        savePreferences();
    });
}

// ── Menu bar ──────────────────────────────────────────────────────────────────

function setupMenuBar() {
    document.querySelectorAll<HTMLElement>(".menu-item").forEach(item => {
        const trigger = item.querySelector<HTMLButtonElement>(".menu-trigger")!;
        trigger.addEventListener("click", (e) => {
            e.stopPropagation();
            const isOpen = item.classList.contains("open");
            closeAllMenus();
            if (!isOpen) item.classList.add("open");
        });
        trigger.addEventListener("mouseenter", () => {
            const anyOpen = document.querySelector(".menu-item.open");
            if (anyOpen && anyOpen !== item) {
                closeAllMenus();
                item.classList.add("open");
            }
        });
    });
    document.addEventListener("click", closeAllMenus);

    // Prevent clicks inside dropdowns from closing them — lets Options controls work inline
    document.querySelectorAll<HTMLElement>(".menu-dropdown").forEach(dd => {
        dd.addEventListener("click", (e) => e.stopPropagation());
    });

    document.querySelectorAll<HTMLButtonElement>(".menu-action").forEach(btn => {
        btn.addEventListener("click", () => { closeAllMenus(); handleMenuAction(btn.dataset.action ?? ""); });
    });
    document.getElementById("btn-about-close")!.addEventListener("click", () => {
        (document.getElementById("dialog-about") as HTMLDialogElement).close();
    });
    document.getElementById("btn-update-close")!.addEventListener("click", () => {
        (document.getElementById("dialog-update") as HTMLDialogElement).close();
    });
    document.getElementById("btn-sysres-close")!.addEventListener("click", () => {
        (document.getElementById("dialog-sysres") as HTMLDialogElement).close();
    });
    document.getElementById("btn-github")!.addEventListener("click", () => {
        openUrl("https://github.com/rustypig91/canvaz");
    });
    document.addEventListener("keydown", (e) => {
        if (e.ctrlKey && !e.shiftKey && e.key === "o") { e.preventDefault(); handleMenuAction("open-project"); }
        if (e.ctrlKey && !e.shiftKey && e.key === "s") { e.preventDefault(); handleMenuAction("save-project"); }
        if (e.ctrlKey && e.shiftKey && e.key === "S") { e.preventDefault(); handleMenuAction("save-as-project"); }
    });

}

function closeAllMenus() {
    document.querySelectorAll(".menu-item.open").forEach(el => el.classList.remove("open"));
}

function handleMenuAction(action: string) {
    switch (action) {
        case "new-project": newProject(); break;
        case "open-project": openProject(); break;
        case "save-project": saveProject(); break;
        case "save-as-project": saveProjectAs(); break;
        case "reload": location.reload(); break;
        case "export-csv": exportCsv(); break;
        case "export-trace-csv": exportTraceCsv(); break;
        case "about":
            invoke<string>("get_version").then(v => { document.getElementById("about-version")!.textContent = v; }).catch(() => { });
            (document.activeElement as HTMLElement)?.blur();
            (document.getElementById("dialog-about") as HTMLDialogElement).showModal();
            break;
        case "system-resources": openSysResDialog(); break;
        case "check-updates": checkForUpdates(true); break;
    }
}

// ── Trace tab ─────────────────────────────────────────────────────────────────

interface TraceEntry {
    channelHandle: number;
    canId: number;
    isExtended: boolean;
    dlc: number;
    data: number[];
    messageName: string | null;
    timestampMs: number;
    cycleTimeMs: number | null;
    direction: "rx" | "tx";
    j1939: J1939Info | null;
    // True if this row is a synthetic frame reassembled from a J1939 TP.CM /
    // TP.DT transfer rather than a frame that actually appeared on the bus.
    reassembled: boolean;
    // Interleaved [value, raw] pairs in DBC message signal order (see CanFrameEvent).
    signals: (number | null)[];
}

// Decoded signal values (interleaved [value, raw] pairs) for each rendered trace
// row, keyed by the row element so the expansion table can show them without
// re-decoding in TS. Auto-cleared when a row is evicted (element GC'd).
const traceRowSignals = new WeakMap<HTMLTableRowElement, (number | null)[]>();

type TraceMode = "overwrite" | "append";
type TraceDataFormat = "hex" | "dec" | "ascii";
let traceMode: TraceMode = "overwrite";
let traceDataFormat: TraceDataFormat = "hex";
let traceTabActive = true; // trace tab is the default active tab (see index.html)
let traceMaxRows = 1000;
let traceHeaderEls: HTMLTableCellElement[] = [];

interface TraceColDef { key: string; label: string; defaultWidth: number; }
const TRACE_COL_DEFS: TraceColDef[] = [
    { key: "ts", label: "Timestamp", defaultWidth: 100 },
    { key: "dir", label: "Dir", defaultWidth: 56 },
    { key: "channel", label: "Channel", defaultWidth: 80 },
    { key: "canId", label: "CAN ID", defaultWidth: 90 },
    { key: "pgn", label: "PGN", defaultWidth: 70 },
    { key: "prio", label: "Prio", defaultWidth: 48 },
    { key: "sa", label: "Src", defaultWidth: 52 },
    { key: "da", label: "Dst", defaultWidth: 52 },
    { key: "msg", label: "Message", defaultWidth: 160 },
    { key: "dlc", label: "DLC", defaultWidth: 56 },
    { key: "data", label: "Data", defaultWidth: 220 },
    { key: "cycle", label: "Cycle (ms)", defaultWidth: 90 },
];
// Columns that only carry data on J1939 channels; they exist in the table
// (order, widths, hidden state all persist) but are only rendered while at
// least one configured channel uses the J1939 protocol.
const J1939_COL_KEYS = new Set(["pgn", "prio", "sa", "da"]);

function anyJ1939Channel(): boolean {
    for (const ch of channels.values()) if (ch.config.protocol === "j1939") return true;
    return ghostChannels.some(g => g.config.protocol === "j1939");
}

// Trace columns to render, in order: user-hidden columns are dropped, and the
// J1939 columns only appear when a J1939 channel is configured.
function visibleTraceCols(): string[] {
    const j1939 = anyJ1939Channel();
    return traceColOrder.filter(k => !traceColHidden.has(k) && (j1939 || !J1939_COL_KEYS.has(k)));
}
let traceColOrder: string[] = TRACE_COL_DEFS.map(d => d.key);
let traceColHidden = new Set<string>();
let traceColWidths: Record<string, number> = {};
let colDragKey: string | null = null;
let colDragGhost: HTMLDivElement | null = null;
let colDropBefore: string | null | undefined = undefined;
let colDropIndicator: HTMLDivElement | null = null;

const traceLastTs = new Map<string, number>();
const traceRowEls = new Map<string, HTMLTableRowElement>();
const tracePendingOverwrite = new Map<string, TraceEntry>(); // accumulates overwrite updates while viewPaused
const traceSeenChannels = new Set<number>();
const traceSeenCanIds = new Set<number>();
const traceSeenMsgNames = new Set<string>();
let traceSeenNoMsg = false;
// Seen J1939 values feed the pgn/prio/sa/da filter menus; frames without J1939
// info (standard frames on a J1939 channel) are represented by -1 / "(non-J1939)".
const traceSeenPgns = new Set<number>();
const traceSeenPrios = new Set<number>();
const traceSeenSas = new Set<number>();
const traceSeenDas = new Set<number>();
let traceSeenNoJ1939 = false;
let traceFilterChannels: Set<number> | null = null;
let traceFilterCanIds: Set<number> | null = null;
let traceFilterMsgNames: Set<string> | null = null;
let traceFilterPgns: Set<number> | null = null;
let traceFilterPrios: Set<number> | null = null;
let traceFilterSas: Set<number> | null = null;
let traceFilterDas: Set<number> | null = null;
// null = any; true = broadcast (PDU2) PGNs only; false = destination-specific only.
let traceFilterBroadcast: boolean | null = null;
// One optional expected value per data byte position; the array length is the
// user-configurable "bytes to check" count (default 8, up to MAX_DATA_FILTER_BYTES).
const MAX_DATA_FILTER_BYTES = 64;
let traceFilterData: (number | null)[] = new Array(8).fill(null);
let traceFilterCycleMin: number | null = null;
let traceFilterCycleMax: number | null = null;
let traceFilterDlcMin: number | null = null;
let traceFilterDlcMax: number | null = null;
let traceFilterDir: Set<string> | null = null;
type TraceSortCol = "ts" | "dir" | "channel" | "canId" | "pgn" | "prio" | "sa" | "da" | "msg" | "dlc" | "data" | "cycle" | null;
let traceSortCol: TraceSortCol = null;
let traceSortDir: "asc" | "desc" = "asc";
let traceLocalBuffer: TraceEntry[] = [];

function traceKey(handle: number, canId: number, direction: "rx" | "tx") {
    return `${handle}::${canId}::${direction}`;
}

function fmtId(canId: number, isExtended: boolean): string {
    return isExtended
        ? canId.toString(16).toUpperCase().padStart(8, "0") + "x"
        : canId.toString(16).toUpperCase().padStart(3, "0") + "h";
}

function fmtData(data: number[]): string {
    switch (traceDataFormat) {
        case "hex": return data.map(b => b.toString(16).toUpperCase().padStart(2, "0")).join(" ");
        case "dec": return data.map(b => b.toString().padStart(3, " ")).join(" ");
        case "ascii": return data.map(b => b >= 32 && b < 127 ? String.fromCharCode(b) : ".").join("");
    }
}

function fmtElapsed(ts: number): string {
    const elapsed = Math.max(0, ts - appStartTime);
    const h = Math.floor(elapsed / 3600000);
    const m = Math.floor((elapsed % 3600000) / 60000);
    const s = Math.floor((elapsed % 60000) / 1000);
    const ms = elapsed % 1000;
    const hPart = h > 0 ? `${h}:` : "";
    return `${hPart}${m.toString().padStart(2, "0")}:${s.toString().padStart(2, "0")}.${ms.toString().padStart(3, "0")}`;
}

function parseByte(s: string): number | null {
    s = s.trim();
    if (!s) return null;
    const n = (s.startsWith("0x") || /[a-fA-F]/.test(s))
        ? parseInt(s.replace(/^0x/i, ""), 16)
        : parseInt(s, 10);
    return (isNaN(n) || n < 0 || n > 255) ? null : n;
}

function traceRowVisible(channelHandle: number, canId: number, bytes: number[], dir: string, cycleMs: number | null, dlc: number, msgName: string | null = null, j1939: J1939Info | null = null): boolean {
    if (traceFilterChannels !== null && !traceFilterChannels.has(channelHandle)) return false;
    if (traceFilterCanIds !== null && !traceFilterCanIds.has(canId)) return false;
    if (traceFilterDir !== null && !traceFilterDir.has(dir)) return false;
    if (traceFilterMsgNames !== null && !traceFilterMsgNames.has(msgName ?? "")) return false;
    // J1939 filters: frames without J1939 info match the -1 "(non-J1939)" key.
    if (traceFilterPgns !== null && !traceFilterPgns.has(j1939 ? j1939.pgn : -1)) return false;
    if (traceFilterPrios !== null && !traceFilterPrios.has(j1939 ? j1939.priority : -1)) return false;
    if (traceFilterSas !== null && !traceFilterSas.has(j1939 ? j1939.sa : -1)) return false;
    if (traceFilterDas !== null && !traceFilterDas.has(j1939 ? j1939.da : -1)) return false;
    if (traceFilterBroadcast !== null && (!j1939 || j1939IsBroadcast(j1939.pgn) !== traceFilterBroadcast)) return false;
    for (let i = 0; i < traceFilterData.length; i++) {
        const expected = traceFilterData[i];
        if (expected === null) continue;
        if (bytes[i] !== expected) return false;
    }
    if (traceFilterDlcMin !== null && dlc < traceFilterDlcMin) return false;
    if (traceFilterDlcMax !== null && dlc > traceFilterDlcMax) return false;
    if (traceFilterCycleMin !== null && (cycleMs === null || cycleMs < traceFilterCycleMin)) return false;
    if (traceFilterCycleMax !== null && (cycleMs === null || cycleMs > traceFilterCycleMax)) return false;
    return true;
}

function applyTraceFilter() {
    const tbody = document.getElementById("trace-tbody") as HTMLTableSectionElement;
    if (traceMode === "append") {
        // Rebuild DOM entirely from the in-memory buffer — never keep invisible rows in the DOM.
        destroyAllTracePlots();
        tbody.innerHTML = "";
        for (const entry of traceLocalBuffer) {
            if (traceRowVisible(entry.channelHandle, entry.canId, entry.data, entry.direction, entry.cycleTimeMs, entry.dlc, entry.messageName, entry.j1939)) {
                tbody.appendChild(buildTraceRow(entry));
            }
        }
        applyTraceSort();
        return;
    }
    // Overwrite mode: toggle visibility on the fixed set of rows.
    for (const tr of Array.from(tbody.rows) as HTMLTableRowElement[]) {
        if ((tr as HTMLTableRowElement).dataset.expand) continue;
        const ch = parseInt(tr.dataset.channelHandle ?? "0");
        const id = parseInt(tr.dataset.canid ?? "0");
        const bytes: number[] = JSON.parse(tr.dataset.bytes ?? "[]");
        const dir = tr.dataset.dir ?? "";
        const cycleMs = tr.dataset.cycle ? parseFloat(tr.dataset.cycle) : null;
        const dlc = parseInt(tr.dataset.dlc ?? "0");
        const msgName = tr.dataset.msg || null;
        const j1939: J1939Info | null = tr.dataset.pgn
            ? {
                pgn: parseInt(tr.dataset.pgn),
                priority: parseInt(tr.dataset.prio ?? "0"),
                sa: parseInt(tr.dataset.sa ?? "0"),
                da: parseInt(tr.dataset.da ?? "255"),
            }
            : null;
        const visible = traceRowVisible(ch, id, bytes, dir, cycleMs, dlc, msgName, j1939);
        tr.style.display = visible ? "" : "none";
        // If hiding a row that has an open expansion, close it
        if (!visible) {
            const next = tr.nextElementSibling as HTMLTableRowElement | null;
            if (next?.dataset.expand) collapseTraceRow(tr, next);
        }
    }
    updateClearFiltersBtn();
    scheduleAutoSave("trace filter changed");
}

// Sort key of one row for the given column, read from the row's datasets.
// Extracted once per row per sort pass (not per comparison) — the sort runs
// after every frame batch, so the comparator must stay cheap.
type TraceSortKey = number | string | number[];
function traceRowSortKey(tr: HTMLTableRowElement, col: Exclude<TraceSortCol, null>): TraceSortKey {
    switch (col) {
        case "ts": return parseInt(tr.dataset.ts ?? "0");
        case "dir": return tr.dataset.dir ?? "";
        case "channel": return channelName(parseInt(tr.dataset.channelHandle ?? "0"));
        case "canId": return parseInt(tr.dataset.canid ?? "0");
        case "pgn": return parseInt(tr.dataset.pgn ?? "-1");
        case "prio": return parseInt(tr.dataset.prio ?? "-1");
        case "sa": return parseInt(tr.dataset.sa ?? "-1");
        case "da": return parseInt(tr.dataset.da ?? "-1");
        case "msg": return tr.dataset.msg ?? "";
        case "dlc": return parseInt(tr.dataset.dlc ?? "0");
        case "data": return JSON.parse(tr.dataset.bytes ?? "[]");
        // Missing cycle sorts last ascending (first descending), as before.
        case "cycle": {
            const c = parseFloat(tr.dataset.cycle ?? "");
            return isNaN(c) ? Infinity : c;
        }
    }
}

function compareSortKeys(a: TraceSortKey, b: TraceSortKey): number {
    if (typeof a === "string") return a.localeCompare(b as string);
    if (Array.isArray(a)) {
        const bb = b as number[];
        for (let i = 0; i < Math.max(a.length, bb.length); i++) {
            const d = (a[i] ?? -1) - (bb[i] ?? -1);
            if (d !== 0) return d;
        }
        return 0;
    }
    if (a === b) return 0; // also covers Infinity vs Infinity (subtraction gives NaN)
    return (a as number) - (b as number);
}

// Re-order the trace rows by the active sort column. Called after every frame
// batch and after bulk rebuilds, so it exits without touching the DOM when the
// rows are already in order. Rows move together with their open expansion row.
function applyTraceSort() {
    if (!traceSortCol) return;
    const col = traceSortCol;
    const tbody = document.getElementById("trace-tbody") as HTMLTableSectionElement;

    const rows = Array.from(tbody.rows).filter(r => !(r as HTMLTableRowElement).dataset.expand) as HTMLTableRowElement[];
    const dir = traceSortDir === "asc" ? 1 : -1;
    const keyed = rows.map(r => ({ r, k: traceRowSortKey(r, col) }));
    // Array.sort is stable, so ties keep their arrival order and don't jitter.
    keyed.sort((a, b) => dir * compareSortKeys(a.k, b.k));

    if (keyed.every((e, i) => e.r === rows[i])) return; // already sorted

    // Capture parent → expansion pairs before moving anything; the sibling
    // relationships change as rows are re-appended.
    const expansions = new Map<HTMLTableRowElement, HTMLTableRowElement>();
    for (const r of Array.from(tbody.querySelectorAll<HTMLTableRowElement>("tr[data-expand]"))) {
        const parent = r.previousElementSibling as HTMLTableRowElement | null;
        if (parent) expansions.set(parent, r);
    }
    const frag = document.createDocumentFragment();
    for (const { r } of keyed) {
        frag.appendChild(r);
        const exp = expansions.get(r);
        if (exp) frag.appendChild(exp);
    }
    tbody.appendChild(frag);
}

function buildTraceCellHtml(key: string, entry: TraceEntry): string {
    const dirClass = entry.direction === "tx" ? "dir-tx" : "dir-rx";
    const j = entry.j1939;
    switch (key) {
        case "pgn": return `<td data-col="pgn" class="td-canid"${j ? ` data-tip="PGN ${j.pgn} (0x${j.pgn.toString(16).toUpperCase()})"` : ""}>${j ? fmtPgn(j.pgn) : "—"}</td>`;
        case "prio": return `<td data-col="prio">${j ? j.priority : "—"}</td>`;
        case "sa": return `<td data-col="sa" class="td-canid">${j ? fmtJ1939Addr(j.sa) : "—"}</td>`;
        case "da": return `<td data-col="da" class="td-canid">${j ? (j.da === 0xFF ? "All" : fmtJ1939Addr(j.da)) : "—"}</td>`;
        case "ts": return `<td data-col="ts" class="td-ts">${fmtElapsed(entry.timestampMs)}</td>`;
        case "dir": return `<td data-col="dir"><span class="dir-badge ${dirClass}">${entry.direction.toUpperCase()}</span></td>`;
        case "channel": return `<td data-col="channel">${escapeHtml(channelName(entry.channelHandle))}</td>`;
        case "canId": return `<td data-col="canId" class="td-canid">${entry.reassembled ? `<span class="tp-badge" data-tip="Reassembled from J1939 TP.CM/TP.DT — this frame did not appear on the bus in this form">${fmtId(entry.canId, entry.isExtended)}</span>` : fmtId(entry.canId, entry.isExtended)}</td>`;
        case "msg": return `<td data-col="msg">${entry.messageName ?? "<em style='color:var(--text-muted)'>-</em>"}</td>`;
        case "dlc": return `<td data-col="dlc">${entry.dlc}</td>`;
        case "data": return `<td data-col="data" class="td-data">${fmtData(entry.data)}</td>`;
        case "cycle": return `<td data-col="cycle" class="td-cycle">${entry.cycleTimeMs != null ? entry.cycleTimeMs.toFixed(1) : "—"}</td>`;
        default: return `<td data-col="${key}"></td>`;
    }
}

function entryFromRow(tr: HTMLTableRowElement): TraceEntry {
    return {
        data: JSON.parse(tr.dataset.bytes ?? "[]"),
        channelHandle: parseInt(tr.dataset.channelHandle ?? "0"),
        canId: parseInt(tr.dataset.canid ?? "0"),
        isExtended: tr.dataset.ext === "1",
        timestampMs: parseInt(tr.dataset.ts ?? "0"),
        direction: (tr.dataset.dir as "rx" | "tx") ?? "rx",
        messageName: tr.dataset.msg || null,
        dlc: parseInt(tr.dataset.dlc ?? "0"),
        cycleTimeMs: tr.dataset.cycle ? parseFloat(tr.dataset.cycle) : null,
        reassembled: tr.dataset.reassembled === "1",
        j1939: tr.dataset.pgn
            ? {
                pgn: parseInt(tr.dataset.pgn),
                priority: parseInt(tr.dataset.prio ?? "0"),
                sa: parseInt(tr.dataset.sa ?? "0"),
                da: parseInt(tr.dataset.da ?? "255"),
            }
            : null,
        signals: traceRowSignals.get(tr) ?? [],
    };
}

function buildTraceRow(entry: TraceEntry): HTMLTableRowElement {
    const tr = document.createElement("tr");
    traceRowSignals.set(tr, entry.signals);
    tr.dataset.bytes = JSON.stringify(entry.data);
    tr.dataset.channelHandle = String(entry.channelHandle);
    tr.dataset.canid = String(entry.canId);
    tr.dataset.ext = entry.isExtended ? "1" : "0";
    tr.dataset.ts = String(entry.timestampMs);
    tr.dataset.dir = entry.direction;
    tr.dataset.msg = entry.messageName ?? "";
    tr.dataset.dlc = String(entry.dlc);
    tr.dataset.cycle = entry.cycleTimeMs != null ? String(entry.cycleTimeMs) : "";
    if (entry.reassembled) tr.dataset.reassembled = "1";
    if (entry.j1939) {
        tr.dataset.pgn = String(entry.j1939.pgn);
        tr.dataset.prio = String(entry.j1939.priority);
        tr.dataset.sa = String(entry.j1939.sa);
        tr.dataset.da = String(entry.j1939.da);
    }
    if (entry.messageName) tr.classList.add("dbc-match");
    if (!traceRowVisible(entry.channelHandle, entry.canId, entry.data, entry.direction, entry.cycleTimeMs, entry.dlc, entry.messageName, entry.j1939)) tr.style.display = "none";
    tr.innerHTML = visibleTraceCols().map(k => buildTraceCellHtml(k, entry)).join("");
    return tr;
}

function updateTraceRowEl(tr: HTMLTableRowElement, entry: TraceEntry) {
    traceRowSignals.set(tr, entry.signals);
    tr.dataset.bytes = JSON.stringify(entry.data);
    tr.dataset.ts = String(entry.timestampMs);
    tr.dataset.dlc = String(entry.dlc);
    tr.dataset.cycle = entry.cycleTimeMs != null ? String(entry.cycleTimeMs) : "";
    if (entry.reassembled) tr.dataset.reassembled = "1"; else delete tr.dataset.reassembled;
    tr.style.display = traceRowVisible(entry.channelHandle, entry.canId, entry.data, entry.direction, entry.cycleTimeMs, entry.dlc, entry.messageName, entry.j1939) ? "" : "none";
    const gc = (k: string) => tr.querySelector<HTMLTableCellElement>(`[data-col="${k}"]`);
    const tsCell = gc("ts"); if (tsCell) tsCell.textContent = fmtElapsed(entry.timestampMs);
    const dlcCell = gc("dlc"); if (dlcCell) dlcCell.textContent = String(entry.dlc);
    const dataCell = gc("data"); if (dataCell) dataCell.textContent = fmtData(entry.data);
    const cycleCell = gc("cycle"); if (cycleCell) cycleCell.textContent = entry.cycleTimeMs != null ? entry.cycleTimeMs.toFixed(1) : "—";
    const canIdCell = gc("canId"); if (canIdCell) canIdCell.outerHTML = buildTraceCellHtml("canId", entry);

    // Refresh open expansion row with updated signal values + current min/max
    const next = tr.nextElementSibling as HTMLTableRowElement | null;
    if (next?.dataset.expand) {
        const msg = dbcMessageFor(entry.channelHandle, entry.canId, entry.isExtended);
        if (msg) {
            const vals = entry.signals; // interleaved [value, raw], index-aligned with msg.signals
            const valCells = next.querySelectorAll<HTMLElement>(".te-val");
            const minCells = next.querySelectorAll<HTMLElement>(".te-min");
            const maxCells = next.querySelectorAll<HTMLElement>(".te-max");
            const enumCells = next.querySelectorAll<HTMLElement>(".te-enum");
            msg.signals.forEach((sig: DbcSignal, i: number) => {
                // Null pairs mark signals of a multiplexer group not active in this frame.
                const hasVal = 2 * i + 1 < vals.length && vals[2 * i] != null;
                if (valCells[i]) {
                    valCells[i].textContent = hasVal ? formatSigValue(vals[2 * i]!, "") : "—";
                    if (hasVal) valCells[i].dataset.tip = `Raw: ${vals[2 * i + 1]}`;
                }
                // Keyed by the DBC message id (not the wire id): J1939 frames from
                // different senders share the same signal series.
                const key = plotKey(entry.channelHandle, msg.id, sig.name);
                const mn = signalMinValues.get(key);
                const mx = signalMaxValues.get(key);
                if (minCells[i]) {
                    minCells[i].textContent = mn !== undefined ? formatSigValue(mn, "") : "—";
                    if (mn !== undefined) minCells[i].dataset.tip = `Raw: ${physToRaw(sig, mn)}`;
                }
                if (maxCells[i]) {
                    maxCells[i].textContent = mx !== undefined ? formatSigValue(mx, "") : "—";
                    if (mx !== undefined) maxCells[i].dataset.tip = `Raw: ${physToRaw(sig, mx)}`;
                }
                if (enumCells[i]) enumCells[i].textContent = hasVal ? (enumLabelForRaw(sig, vals[2 * i + 1]!) || "—") : "—";
            });
        }
    }
}

function onCanFrameBatch(events: CanFrameEvent[]) {
    if (!appRunning || events.length === 0) return;

    // Signals touched in this batch; sidebar DOM is written once per signal per
    // batch (the maps below always hold the latest value) instead of per frame.
    const dirtySignals = new Set<string>();
    // Overwrite mode: only the newest entry per trace key needs a DOM update.
    const latestOverwrite = new Map<string, TraceEntry>();
    const appendEntries: TraceEntry[] = [];

    for (const ev of events) {
        // Message identity comes from the frontend's copy of the DBC (the event
        // carries no strings); both sides parsed the same file at open. On J1939
        // channels the lookup falls back to PGN + source-address matching,
        // mirroring the backend.
        const msg = dbcMessageFor(ev.channel_handle, ev.can_id, ev.is_extended);
        const messageName = msg?.name ?? null;

        // Update seen sets (for filter autocomplete) and cycle timing.
        traceSeenChannels.add(ev.channel_handle);
        traceSeenCanIds.add(ev.can_id);
        if (messageName) traceSeenMsgNames.add(messageName);
        else traceSeenNoMsg = true;
        if (ev.j1939) {
            traceSeenPgns.add(ev.j1939.pgn);
            traceSeenPrios.add(ev.j1939.priority);
            traceSeenSas.add(ev.j1939.sa);
            traceSeenDas.add(ev.j1939.da);
        } else {
            traceSeenNoJ1939 = true;
        }

        const direction = ev.direction ?? "rx";
        const key = traceKey(ev.channel_handle, ev.can_id, direction);
        const prev = traceLastTs.get(key);
        const cycleTime = prev != null ? ev.timestamp_ms - prev : null;
        traceLastTs.set(key, ev.timestamp_ms);

        // Process decoded signals (interleaved [value, raw] pairs, index-aligned
        // with msg.signals) — update value maps and plot series regardless of
        // pause state (frozen charts display a snapshot, data keeps flowing).
        if (msg && ev.signals.length > 0) {
            const keys = sigKeysFor(ev.channel_handle, msg);
            const x = (ev.timestamp_ms - appStartTime) / 1000;
            const n = Math.min(keys.length, ev.signals.length >>> 1);
            for (let i = 0; i < n; i++) {
                const value = ev.signals[2 * i];
                if (value == null) continue; // inactive multiplexer group in this frame
                const raw = ev.signals[2 * i + 1]!;
                const sigKey = keys[i];
                signalLastValues.set(sigKey, value);
                signalLastRaw.set(sigKey, raw);
                const mn = signalMinValues.get(sigKey);
                if (mn === undefined || value < mn) signalMinValues.set(sigKey, value);
                const mx = signalMaxValues.get(sigKey);
                if (mx === undefined || value > mx) signalMaxValues.set(sigKey, value);
                dirtySignals.add(sigKey);
                for (const pane of plotPanes) {
                    const series = pane.series.get(sigKey);
                    if (!series) continue;
                    if (series.timestamps.length > 0 &&
                        series.timestamps[series.timestamps.length - 1] >= ev.timestamp_ms) continue;
                    series.timestamps.push(ev.timestamp_ms);
                    series.data.push({ x, y: value });
                    series.lastValue = value;
                    markPaneDirty(pane); // no-op while viewPaused; data accumulates in series
                }
                if (tracePlotSigKeys.has(sigKey)) pushTracePlotPoint(sigKey, ev.timestamp_ms, x, value);
            }
        }

        const entry: TraceEntry = {
            channelHandle: ev.channel_handle,
            canId: ev.can_id,
            isExtended: ev.is_extended,
            dlc: ev.dlc,
            data: ev.data,
            messageName,
            timestampMs: ev.timestamp_ms,
            cycleTimeMs: cycleTime,
            direction,
            j1939: ev.j1939 ?? null,
            reassembled: ev.reassembled ?? false,
            signals: ev.signals,
        };

        if (viewPaused) {
            // Accumulate latest state per key; DOM stays frozen until resume.
            if (traceMode === "overwrite") tracePendingOverwrite.set(key, entry);
            continue;
        }

        if (traceMode === "overwrite") latestOverwrite.set(key, entry);
        // Append rows are only built while the trace tab is visible; switching to
        // the tab reloads the buffer from the backend and rebuilds the table.
        else if (traceTabActive) appendEntries.push(entry);
    }

    // Sidebar: one DOM write per signal per batch, from the latest values.
    // Frozen while the view is paused — the maps keep tracking underneath and
    // resumeFromPause() brings the rows back up to date.
    if (!viewPaused) {
        for (const sigKey of dirtySignals) {
            const valEl = signalValueEls.get(sigKey);
            if (valEl) {
                valEl.textContent = sidebarValueText(sigKey, signalLastValues.get(sigKey)!, signalLastRaw.get(sigKey), signalUnits.get(sigKey) ?? "");
                valEl.classList.remove("sig-value--empty");
            }
            const rangeEl = signalRangeEls.get(sigKey);
            if (rangeEl) {
                rangeEl.textContent = `↓${formatSigValue(signalMinValues.get(sigKey)!, "")} ↑${formatSigValue(signalMaxValues.get(sigKey)!, "")}`;
                rangeEl.classList.remove("sig-value--empty");
            }
        }
    }

    if (latestOverwrite.size > 0) {
        const tbody = document.getElementById("trace-tbody") as HTMLTableSectionElement;
        for (const [key, entry] of latestOverwrite) {
            const existing = traceRowEls.get(key);
            if (existing) {
                updateTraceRowEl(existing, entry);
            } else {
                const tr = buildTraceRow(entry);
                traceRowEls.set(key, tr);
                tbody.appendChild(tr);
            }
        }
    }

    if (appendEntries.length > 0) {
        const tbody = document.getElementById("trace-tbody") as HTMLTableSectionElement;
        // Events arrive oldest→newest; insert in reverse so the newest ends up on top.
        const frag = document.createDocumentFragment();
        for (let i = appendEntries.length - 1; i >= 0; i--) {
            const e = appendEntries[i];
            if (traceRowVisible(e.channelHandle, e.canId, e.data, e.direction, e.cycleTimeMs, e.dlc, e.messageName, e.j1939)) {
                frag.appendChild(buildTraceRow(e));
            }
        }
        tbody.insertBefore(frag, tbody.firstChild);
        // Cap the row count by dropping the oldest rows. Unsorted, the oldest
        // sit at the bottom; with an active column sort the bottom row is just
        // whatever sorts last, so evict by timestamp instead.
        if (!traceSortCol) {
            while (tbody.rows.length > traceMaxRows) tbody.deleteRow(-1);
        } else if (tbody.rows.length > traceMaxRows) {
            const rows = (Array.from(tbody.rows) as HTMLTableRowElement[]).filter(r => !r.dataset.expand);
            rows.sort((a, b) => parseInt(a.dataset.ts ?? "0") - parseInt(b.dataset.ts ?? "0"));
            const excess = tbody.rows.length - traceMaxRows;
            for (let i = 0; i < excess && i < rows.length; i++) {
                const next = rows[i].nextElementSibling as HTMLTableRowElement | null;
                if (next?.dataset.expand) next.remove();
                rows[i].remove();
            }
        }
    }

    // Keep the user's column sort applied as rows arrive and update in place
    // (no-op when no sort is active or the order is already correct).
    if (latestOverwrite.size > 0 || appendEntries.length > 0) applyTraceSort();
}

// Rebuild the interleaved [value, raw] layout from get_frames' named signals.
// The backend stores only the signals active in each frame, so for multiplexed
// messages the list can be a subset of the DBC's — align by name against the
// message's signal order and fill the gaps with null pairs, mirroring the
// live-event layout.
function interleaveFrameSignals(f: FrameInfo): (number | null)[] {
    const msg = dbcMessageFor(f.channel_handle, f.can_id, f.is_extended);
    if (!msg || msg.signals.length === f.signals.length) {
        return f.signals.flatMap(s => [s.value, s.raw]);
    }
    const byName = new Map(f.signals.map(s => [s.name, s]));
    return msg.signals.flatMap(sig => {
        const d = byName.get(sig.name);
        return d ? [d.value, d.raw] : [null, null];
    });
}

async function loadTraceFrames() {
    const frames = await invoke<FrameInfo[]>("get_frames", { handle: null, limit: traceMaxRows });
    const cycleTimes = new Map<string, number>();
    // Backend returns oldest-first; we want newest-first in traceLocalBuffer.
    traceLocalBuffer = frames.map(f => {
        const k = traceKey(f.channel_handle, f.can_id, f.direction);
        const prev = cycleTimes.get(k);
        const cycleTimeMs = prev != null ? f.timestamp_ms - prev : null;
        cycleTimes.set(k, f.timestamp_ms);
        if (f.message_name) traceSeenMsgNames.add(f.message_name);
        traceSeenChannels.add(f.channel_handle);
        traceSeenCanIds.add(f.can_id);
        if (f.j1939) {
            traceSeenPgns.add(f.j1939.pgn);
            traceSeenPrios.add(f.j1939.priority);
            traceSeenSas.add(f.j1939.sa);
            traceSeenDas.add(f.j1939.da);
        } else {
            traceSeenNoJ1939 = true;
        }
        return {
            channelHandle: f.channel_handle,
            canId: f.can_id,
            isExtended: f.is_extended,
            dlc: f.dlc,
            data: f.data,
            messageName: f.message_name,
            timestampMs: f.timestamp_ms,
            cycleTimeMs,
            direction: f.direction,
            j1939: f.j1939 ?? null,
            reassembled: f.reassembled ?? false,
            // get_frames returns named signals; flatten to the interleaved
            // [value, raw] layout used everywhere else (same DBC order).
            signals: interleaveFrameSignals(f),
        };
    }).reverse();
}

function clearTrace() {
    destroyAllTracePlots();
    (document.getElementById("trace-tbody") as HTMLTableSectionElement).innerHTML = "";
    traceRowEls.clear();
    tracePendingOverwrite.clear();
    traceLastTs.clear();
    traceSeenChannels.clear();
    traceSeenCanIds.clear();
    traceSeenMsgNames.clear();
    traceSeenNoMsg = false;
    traceSeenPgns.clear();
    traceSeenPrios.clear();
    traceSeenSas.clear();
    traceSeenDas.clear();
    traceSeenNoJ1939 = false;
    traceLocalBuffer = [];
}

function refreshTraceFormat() {
    const tbody = document.getElementById("trace-tbody") as HTMLTableSectionElement;
    for (const tr of Array.from(tbody.rows)) {
        const bytes: number[] = JSON.parse((tr as HTMLTableRowElement).dataset.bytes ?? "[]");
        const dc = (tr as HTMLTableRowElement).querySelector<HTMLTableCellElement>('[data-col="data"]');
        if (dc) dc.textContent = fmtData(bytes);
    }
}

function anyFilterActive(): boolean {
    return traceFilterChannels !== null
        || traceFilterCanIds !== null
        || traceFilterMsgNames !== null
        || traceFilterDir !== null
        || traceFilterPgns !== null
        || traceFilterPrios !== null
        || traceFilterSas !== null
        || traceFilterDas !== null
        || traceFilterBroadcast !== null
        || traceFilterDlcMin !== null
        || traceFilterDlcMax !== null
        || traceFilterCycleMin !== null
        || traceFilterCycleMax !== null
        || traceFilterData.some(v => v !== null);
}

function updateClearFiltersBtn() {
    const btn = document.getElementById("btn-clear-filters") as HTMLButtonElement | null;
    if (btn) btn.style.display = anyFilterActive() ? "" : "none";
}

function clearAllFilters() {
    traceFilterChannels = null;
    traceFilterCanIds = null;
    traceFilterMsgNames = null;
    traceFilterDir = null;
    traceFilterPgns = null;
    traceFilterPrios = null;
    traceFilterSas = null;
    traceFilterDas = null;
    traceFilterBroadcast = null;
    traceFilterDlcMin = null;
    traceFilterDlcMax = null;
    traceFilterCycleMin = null;
    traceFilterCycleMax = null;
    traceFilterData.fill(null);
    syncFilteredHeaders();
    applyTraceFilter();
}

function updateSortIndicators() {
    for (const def of TRACE_COL_DEFS) {
        const th = document.querySelector<HTMLElement>(`#trace-table thead th[data-col="${def.key}"]`);
        if (!th) continue;
        const node = th.childNodes[0];
        if (node) node.textContent = def.label + (traceSortCol === def.key ? (traceSortDir === "asc" ? " ▲" : " ▼") : "");
    }
}

function calcColDropTarget(clientX: number, dragKey: string): string | null {
    const ths = Array.from(document.querySelectorAll<HTMLTableCellElement>("#trace-table thead th"));
    for (const th of ths) {
        if (th.dataset.col === dragKey) continue;
        const rect = th.getBoundingClientRect();
        if (clientX < rect.left + rect.width / 2) return th.dataset.col ?? null;
    }
    return null;
}

function updateColDropIndicator(before: string | null) {
    if (!colDropIndicator) {
        colDropIndicator = document.createElement("div");
        colDropIndicator.id = "col-drop-indicator";
        document.body.appendChild(colDropIndicator);
    }
    const ths = Array.from(document.querySelectorAll<HTMLTableCellElement>("#trace-table thead th"));
    let x: number;
    if (before === null) {
        const last = ths[ths.length - 1];
        x = last ? last.getBoundingClientRect().right : 0;
    } else {
        const th = ths.find(t => t.dataset.col === before);
        x = th ? th.getBoundingClientRect().left : 0;
    }
    const trRect = document.querySelector("#trace-table thead tr")!.getBoundingClientRect();
    colDropIndicator.style.left = `${x}px`;
    colDropIndicator.style.top = `${trRect.top}px`;
    colDropIndicator.style.height = `${trRect.height}px`;
    colDropIndicator.style.display = "block";
}

function hideColDropIndicator() {
    if (colDropIndicator) colDropIndicator.style.display = "none";
}

// The one column rendered WITHOUT a width. The table is `table-layout: fixed;
// width: 100%`: when every column has a specified width and the sum doesn't
// match the table width, the browser rescales all columns proportionally —
// specified and rendered widths then disagree, which makes resize drags jump
// and overshoot the mouse. Keeping exactly one width-less column absorbs the
// slack so every other column renders at exactly its specified width.
//
// It must be the RIGHTMOST visible column: a drag on any handle left of the
// absorber is compensated by the absorber, so all edges left of it track the
// mouse 1:1. A column right of the absorber would instead have its left edge
// shift while the dragged right edge stays put.
function traceAbsorberCol(visible: string[]): string {
    return visible[visible.length - 1] ?? "";
}

function rebuildTraceColumns() {
    const visible = visibleTraceCols();
    const absorber = traceAbsorberCol(visible);

    const colgroup = document.querySelector("#trace-table colgroup")!;
    colgroup.innerHTML = visible.map(k => {
        if (k === absorber) return `<col>`;
        const w = traceColWidths[k] ?? TRACE_COL_DEFS.find(d => d.key === k)!.defaultWidth;
        return w ? `<col style="width:${w}px">` : `<col>`;
    }).join("");

    const headerRow = document.querySelector("#trace-table thead tr")!;
    headerRow.innerHTML = visible.map(k => {
        const def = TRACE_COL_DEFS.find(d => d.key === k)!;
        return `<th data-col="${k}">${def.label}</th>`;
    }).join("");
    traceHeaderEls = Array.from(headerRow.children) as HTMLTableCellElement[];

    const tbody = document.getElementById("trace-tbody") as HTMLTableSectionElement;
    for (const tr of Array.from(tbody.rows)) {
        const row = tr as HTMLTableRowElement;
        if (row.dataset.expand) { row.cells[0].colSpan = visible.length; continue; }
        const entry = entryFromRow(row);
        row.innerHTML = visible.map(k => buildTraceCellHtml(k, entry)).join("");
    }

    setupTraceHeaders();
}

function setupTraceHeaders() {
    const ths = traceHeaderEls;
    const traceCols = Array.from(document.querySelectorAll<HTMLElement>("#trace-table colgroup col"));
    const absorber = traceAbsorberCol(visibleTraceCols());

    ths.forEach((th, i) => {
        const key = th.dataset.col ?? "";

        // Resize handle. The absorber column gets none: its width is whatever
        // the fixed columns leave over, so it cannot be sized directly.
        if (key !== absorber) {
            const handle = document.createElement("span");
            handle.className = "col-resizer";
            handle.addEventListener("click", (e) => e.stopPropagation());
            handle.addEventListener("mousedown", (e) => {
                e.preventDefault(); e.stopPropagation();
                // Freeze every fixed column at its currently rendered width
                // before the drag. If any specified width disagrees with the
                // rendered one (widths restored from a project saved at another
                // window size, or an over-full table), the first width write
                // would rescale the whole layout at once: the table jumps and
                // the drag no longer tracks the mouse 1:1.
                ths.forEach((h, j) => {
                    const k = h.dataset.col ?? "";
                    if (k === absorber || !traceCols[j]) return;
                    const wj = h.offsetWidth;
                    traceCols[j].style.width = `${wj}px`;
                    traceColWidths[k] = wj;
                });
                const startX = e.clientX, startW = th.offsetWidth;
                handle.classList.add("active");
                document.body.classList.add("col-resizing");
                const onMove = (ev: MouseEvent) => {
                    const w = Math.max(40, startW + ev.clientX - startX);
                    if (traceCols[i]) traceCols[i].style.width = `${w}px`;
                    traceColWidths[key] = w;
                };
                const onUp = () => {
                    handle.classList.remove("active"); document.body.classList.remove("col-resizing");
                    document.removeEventListener("mousemove", onMove);
                    document.removeEventListener("mouseup", onUp);
                };
                document.addEventListener("mousemove", onMove);
                document.addEventListener("mouseup", onUp);
            });
            th.appendChild(handle);
        }

        // Sort on click
        th.addEventListener("click", () => {
            if (traceSortCol === key) traceSortDir = traceSortDir === "asc" ? "desc" : "asc";
            else { traceSortCol = key as TraceSortCol; traceSortDir = "asc"; }
            updateSortIndicators();
            applyTraceSort();
        });

        // Drag to reorder
        th.addEventListener("mousedown", (e) => {
            if ((e.target as HTMLElement).closest(".col-resizer")) return;
            const startX = e.clientX;
            let dragging = false;
            const onMove = (ev: MouseEvent) => {
                if (!dragging && Math.abs(ev.clientX - startX) >= 5) {
                    dragging = true;
                    colDragKey = key;
                    colDragGhost = document.createElement("div");
                    colDragGhost.className = "col-drag-ghost";
                    colDragGhost.textContent = TRACE_COL_DEFS.find(d => d.key === key)?.label ?? key;
                    document.body.appendChild(colDragGhost);
                    document.body.classList.add("col-resizing");
                }
                if (!dragging) return;
                colDragGhost!.style.left = `${ev.clientX + 12}px`;
                colDragGhost!.style.top = `${ev.clientY - 8}px`;
                const before = calcColDropTarget(ev.clientX, key);
                if (before !== colDropBefore) { colDropBefore = before; updateColDropIndicator(before); }
            };
            const onUp = () => {
                document.removeEventListener("mousemove", onMove);
                document.removeEventListener("mouseup", onUp);
                colDragGhost?.remove(); colDragGhost = null;
                hideColDropIndicator();
                document.body.classList.remove("col-resizing");
                if (dragging && colDragKey) {
                    const before = colDropBefore;
                    colDragKey = null; colDropBefore = undefined;
                    if (before !== key) {
                        traceColOrder = traceColOrder.filter(k => k !== key);
                        const idx = before === null ? traceColOrder.length : traceColOrder.indexOf(before as string);
                        traceColOrder.splice(idx >= 0 ? idx : traceColOrder.length, 0, key);
                        rebuildTraceColumns();
                        scheduleAutoSave("trace column reordered");
                    }
                } else { colDragKey = null; colDropBefore = undefined; }
            };
            document.addEventListener("mousemove", onMove);
            document.addEventListener("mouseup", onUp);
        });

        // Filter context menus
        if (key === "msg") {
            th.addEventListener("contextmenu", (e) => {
                e.preventDefault();
                const items = [...traceSeenMsgNames].sort().map(n => ({ label: n, key: n }));
                if (traceSeenNoMsg) items.push({ label: "(no message)", key: "" });
                if (!items.length) return;
                showFilterMenu(e.clientX, e.clientY, items, traceFilterMsgNames, (active) => {
                    traceFilterMsgNames = active; syncFilteredHeaders(); applyTraceFilter();
                });
            });
        } else if (key === "channel") {
            th.addEventListener("contextmenu", (e) => {
                e.preventDefault();
                const items = [...traceSeenChannels].sort((a, b) => a - b).map(h => ({ label: channelName(h), key: String(h) }));
                if (!items.length) return;
                showFilterMenu(e.clientX, e.clientY, items,
                    traceFilterChannels !== null ? new Set([...traceFilterChannels].map(String)) : null,
                    (active) => {
                        traceFilterChannels = active !== null ? new Set([...active].map(Number)) : null;
                        syncFilteredHeaders(); applyTraceFilter();
                    });
            });
        } else if (key === "canId") {
            th.addEventListener("contextmenu", (e) => {
                e.preventDefault();
                const items = [...traceSeenCanIds].sort((a, b) => a - b).map(id => ({ label: fmtId(id, id > 0x7FF), key: String(id) }));
                if (!items.length) return;
                showFilterMenu(e.clientX, e.clientY, items,
                    traceFilterCanIds !== null ? new Set([...traceFilterCanIds].map(String)) : null,
                    (active) => {
                        traceFilterCanIds = active !== null ? new Set([...active].map(Number)) : null;
                        syncFilteredHeaders(); applyTraceFilter();
                    });
            });
        } else if (key === "pgn") {
            th.addEventListener("contextmenu", (e) => {
                e.preventDefault();
                const items = [...traceSeenPgns].sort((a, b) => a - b)
                    .map(p => ({ label: `${fmtPgn(p)}${j1939IsBroadcast(p) ? "" : " (dest.)"}`, key: String(p) }));
                if (traceSeenNoJ1939) items.push({ label: "(non-J1939)", key: "-1" });
                if (!items.length) return;
                // Broadcast / destination-specific selector shown above the PGN list.
                const bRow = document.createElement("div");
                bRow.className = "data-fmt-row";
                for (const [value, label] of [[null, "Any"], [true, "Broadcast"], [false, "Dest."]] as const) {
                    const btn = document.createElement("button");
                    btn.textContent = label;
                    btn.className = "data-fmt-btn" + (traceFilterBroadcast === value ? " active" : "");
                    btn.addEventListener("click", () => {
                        traceFilterBroadcast = value;
                        bRow.querySelectorAll<HTMLButtonElement>(".data-fmt-btn").forEach(b => b.classList.remove("active"));
                        btn.classList.add("active");
                        syncFilteredHeaders(); applyTraceFilter();
                    });
                    bRow.appendChild(btn);
                }
                showFilterMenu(e.clientX, e.clientY, items,
                    traceFilterPgns !== null ? new Set([...traceFilterPgns].map(String)) : null,
                    (active) => {
                        traceFilterPgns = active !== null ? new Set([...active].map(Number)) : null;
                        syncFilteredHeaders(); applyTraceFilter();
                    }, bRow);
            });
        } else if (key === "prio" || key === "sa" || key === "da") {
            // Shared seen-value filter for the remaining J1939 columns.
            const cfg = {
                prio: {
                    seen: traceSeenPrios, fmt: (v: number) => String(v),
                    get: () => traceFilterPrios, set: (v: Set<number> | null) => { traceFilterPrios = v; },
                },
                sa: {
                    seen: traceSeenSas, fmt: fmtJ1939Addr,
                    get: () => traceFilterSas, set: (v: Set<number> | null) => { traceFilterSas = v; },
                },
                da: {
                    seen: traceSeenDas, fmt: (v: number) => v === 0xFF ? "All (FFh)" : fmtJ1939Addr(v),
                    get: () => traceFilterDas, set: (v: Set<number> | null) => { traceFilterDas = v; },
                },
            }[key];
            th.addEventListener("contextmenu", (e) => {
                e.preventDefault();
                const items = [...cfg.seen].sort((a, b) => a - b).map(v => ({ label: cfg.fmt(v), key: String(v) }));
                if (traceSeenNoJ1939) items.push({ label: "(non-J1939)", key: "-1" });
                if (!items.length) return;
                const current = cfg.get();
                showFilterMenu(e.clientX, e.clientY, items,
                    current !== null ? new Set([...current].map(String)) : null,
                    (active) => {
                        cfg.set(active !== null ? new Set([...active].map(Number)) : null);
                        syncFilteredHeaders(); applyTraceFilter();
                    });
            });
        } else if (key === "data") {
            th.addEventListener("contextmenu", (e) => {
                e.preventDefault();
                if (ctxMenu) ctxMenu.remove();
                const menu = document.createElement("div");
                menu.className = "ctx-menu data-filter-menu";
                menu.addEventListener("click", ev => ev.stopPropagation());
                const fmtRow = document.createElement("div");
                fmtRow.className = "data-fmt-row";
                for (const [value, label] of [["hex", "Hex"], ["dec", "Dec"], ["ascii", "ASCII"]] as const) {
                    const btn = document.createElement("button");
                    btn.textContent = label;
                    btn.className = "data-fmt-btn" + (traceDataFormat === value ? " active" : "");
                    btn.addEventListener("click", () => {
                        traceDataFormat = value;
                        fmtRow.querySelectorAll<HTMLButtonElement>(".data-fmt-btn").forEach(b => b.classList.remove("active"));
                        btn.classList.add("active"); refreshTraceFormat();
                        scheduleAutoSave("trace data format changed");
                    });
                    fmtRow.appendChild(btn);
                }
                menu.appendChild(fmtRow);
                const sep = document.createElement("div"); sep.className = "data-filter-sep"; menu.appendChild(sep);

                // How many data byte positions to filter on. Default 8; raise it
                // to match reassembled J1939 transport messages (dlc > 8).
                const countRow = document.createElement("div"); countRow.className = "range-filter-row";
                const countLbl = document.createElement("span"); countLbl.className = "range-filter-lbl"; countLbl.textContent = "Bytes:";
                const countInp = document.createElement("input");
                countInp.type = "number"; countInp.min = "1"; countInp.max = String(MAX_DATA_FILTER_BYTES);
                countInp.className = "range-filter-inp";
                countInp.value = String(traceFilterData.length);
                countRow.append(countLbl, countInp);
                menu.appendChild(countRow);

                const inputs: HTMLInputElement[] = [];
                const grid = document.createElement("div"); grid.className = "data-filter-grid";
                grid.style.maxHeight = "40vh"; grid.style.overflowY = "auto";
                const buildGrid = () => {
                    grid.innerHTML = "";
                    inputs.length = 0;
                    for (let idx = 0; idx < traceFilterData.length; idx++) {
                        const cell = document.createElement("div"); cell.className = "data-filter-cell";
                        const lbl = document.createElement("span"); lbl.className = "data-filter-lbl"; lbl.textContent = String(idx);
                        const inp = document.createElement("input");
                        inp.type = "text"; inp.className = "data-filter-inp"; inp.placeholder = "—"; inp.maxLength = 4;
                        const cur = traceFilterData[idx];
                        if (cur !== null) inp.value = cur.toString(16).toUpperCase().padStart(2, "0");
                        inp.addEventListener("input", () => {
                            const val = parseByte(inp.value);
                            traceFilterData[idx] = val;
                            inp.classList.toggle("data-filter-invalid", inp.value.trim() !== "" && val === null);
                            syncFilteredHeaders(); applyTraceFilter();
                        });
                        cell.append(lbl, inp); grid.appendChild(cell); inputs.push(inp);
                    }
                };
                buildGrid();
                countInp.addEventListener("change", () => {
                    const n = Math.max(1, Math.min(MAX_DATA_FILTER_BYTES, parseInt(countInp.value) || 8));
                    countInp.value = String(n);
                    if (n === traceFilterData.length) return;
                    // Values at surviving positions are kept; truncated positions
                    // lose their filter.
                    traceFilterData = Array.from({ length: n }, (_, i) => traceFilterData[i] ?? null);
                    buildGrid();
                    syncFilteredHeaders();
                    applyTraceFilter();
                    scheduleAutoSave("trace data filter length changed");
                });
                menu.appendChild(grid);
                const hint = document.createElement("div"); hint.className = "data-filter-hint";
                hint.textContent = "hex (FF) or decimal (255), empty = any"; menu.appendChild(hint);
                const clearBtn = document.createElement("button");
                clearBtn.textContent = "Clear all"; clearBtn.className = "filter-ctrl-btn";
                clearBtn.style.marginTop = "6px"; clearBtn.style.width = "100%";
                clearBtn.addEventListener("click", () => {
                    traceFilterData.fill(null);
                    inputs.forEach(inp => { inp.value = ""; inp.classList.remove("data-filter-invalid"); });
                    syncFilteredHeaders(); applyTraceFilter();
                });
                menu.appendChild(clearBtn);
                menu.style.left = `${e.clientX}px`; menu.style.top = `${e.clientY}px`;
                document.body.appendChild(menu); ctxMenu = menu;
                const rect = menu.getBoundingClientRect();
                if (rect.right > window.innerWidth) menu.style.left = `${e.clientX - rect.width}px`;
                if (rect.bottom > window.innerHeight) menu.style.top = `${e.clientY - rect.height}px`;
            });
        } else if (key === "dir") {
            th.addEventListener("contextmenu", (e) => {
                e.preventDefault();
                showFilterMenu(e.clientX, e.clientY,
                    [{ label: "RX", key: "rx" }, { label: "TX", key: "tx" }],
                    traceFilterDir, (active) => { traceFilterDir = active; syncFilteredHeaders(); applyTraceFilter(); });
            });
        } else if (key === "dlc") {
            th.addEventListener("contextmenu", (e) => {
                e.preventDefault();
                showRangeFilterMenu(e.clientX, e.clientY, "DLC", traceFilterDlcMin, traceFilterDlcMax, (mn, mx) => {
                    traceFilterDlcMin = mn; traceFilterDlcMax = mx; syncFilteredHeaders(); applyTraceFilter();
                });
            });
        } else if (key === "cycle") {
            th.addEventListener("contextmenu", (e) => {
                e.preventDefault();
                showRangeFilterMenu(e.clientX, e.clientY, "Cycle (ms)", traceFilterCycleMin, traceFilterCycleMax, (mn, mx) => {
                    traceFilterCycleMin = mn; traceFilterCycleMax = mx; syncFilteredHeaders(); applyTraceFilter();
                });
            });
        }
    });

    updateSortIndicators();
    syncFilteredHeaders();
}

// ── Inline trace plots ────────────────────────────────────────────────────────
// Clicking a signal row inside a trace row expansion opens a live plot of that
// signal in a fixed-height row inserted directly under it. Each signal of a
// message can have its own plot open at the same time; clicking the signal
// again closes it. The charts scroll continuously (requestAnimationFrame),
// like the panes in the plot tab.

interface TracePlot {
    chart: Chart;
    sigKey: string;
    data: { x: number; y: number }[];
    lastTs: number;
    /// Highlighted signal row the plot sits under.
    sigRow: HTMLTableRowElement;
}

// Keyed by the plot's own <tr> inside the expansion's signal table.
const tracePlots = new Map<HTMLTableRowElement, TracePlot>();
// Membership test used in the hot per-frame decode loop.
let tracePlotSigKeys = new Set<string>();
let tracePlotRaf: number | null = null;

function rebuildTracePlotKeys() {
    tracePlotSigKeys = new Set([...tracePlots.values()].map(tp => tp.sigKey));
}

function pushTracePlotPoint(sigKey: string, tsMs: number, x: number, value: number) {
    for (const tp of tracePlots.values()) {
        if (tp.sigKey !== sigKey || tsMs <= tp.lastTs) continue;
        tp.lastTs = tsMs;
        tp.data.push({ x, y: value });
    }
}

function destroyTracePlot(plotTr: HTMLTableRowElement) {
    const tp = tracePlots.get(plotTr);
    if (!tp) return;
    tp.chart.destroy();
    tp.sigRow.classList.remove("te-plotted");
    tracePlots.delete(plotTr);
    plotTr.remove();
    rebuildTracePlotKeys();
}

function destroyAllTracePlots() {
    for (const tp of tracePlots.values()) tp.chart.destroy();
    tracePlots.clear();
    rebuildTracePlotKeys();
}

// Continuous scroll loop for the inline plots, mirroring the plot tab: advance
// the x window by wall time every animation frame and redraw. Also
// garbage-collects plots whose row left the DOM through a path without an
// explicit destroy (row eviction, bulk table rebuilds).
function tracePlotTick() {
    if (tracePlots.size === 0) {
        tracePlotRaf = null;
        return;
    }
    const now = (Date.now() - appStartTime) / 1000;
    const cutoffX = now - windowSizeSec;
    let removed = false;
    for (const [plotTr, tp] of tracePlots) {
        if (!plotTr.isConnected) {
            tp.chart.destroy();
            tracePlots.delete(plotTr);
            removed = true;
            continue;
        }
        // Stopped, frozen (pause) or hidden (other tab): no scrolling; data
        // keeps accumulating and the chart catches up when visible again.
        if (!appRunning || viewPaused || !traceTabActive) continue;
        while (tp.data.length > 0 && tp.data[0].x < cutoffX) tp.data.shift();
        const xs = (tp.chart.options.scales as any)["x"];
        xs.min = Math.max(0, now - windowSizeSec);
        xs.max = Math.max(windowSizeSec, now);
        tp.chart.update("none");
    }
    if (removed) rebuildTracePlotKeys();
    tracePlotRaf = requestAnimationFrame(tracePlotTick);
}

function startTracePlotLoop() {
    if (tracePlotRaf === null) tracePlotRaf = requestAnimationFrame(tracePlotTick);
}

async function toggleTracePlot(sigRow: HTMLTableRowElement, handle: number, msgId: number, sig: DbcSignal) {
    // A plot row directly under the signal row means it's open: toggle off.
    const next = sigRow.nextElementSibling as HTMLTableRowElement | null;
    if (next?.dataset.sigplot) {
        destroyTracePlot(next);
        return;
    }

    // Inline plots only work in overwrite mode, where the signal row is a
    // stable slot that keeps receiving updates. In append mode each row is a
    // historical snapshot that scrolls away, so there is nothing to plot onto.
    if (traceMode !== "overwrite") {
        log("warn", "Inline plots require Overwrite mode.");
        return;
    }

    // Full-width, fixed-height plot row under the clicked signal. The signal
    // table spans the whole expansion width (filler column), so the plot cell
    // simply fills the row.
    const plotTr = document.createElement("tr");
    plotTr.dataset.sigplot = "1";
    const td = document.createElement("td");
    td.colSpan = sigRow.cells.length;
    td.className = "te-plot-cell";
    const container = document.createElement("div");
    container.className = "te-plot";
    const canvas = document.createElement("canvas");
    container.appendChild(canvas);
    td.appendChild(container);
    plotTr.appendChild(td);
    sigRow.after(plotTr);

    const sigKey = plotKey(handle, msgId, sig.name);
    const data: { x: number; y: number }[] = [];
    const now = (Date.now() - appStartTime) / 1000;
    const chart = new Chart(canvas, {
        type: "line",
        data: {
            datasets: [{
                label: sig.name,
                data,
                borderColor: PLOT_COLORS[0],
                backgroundColor: "transparent",
                borderWidth: 1.5,
                // Every sample is marked and the line steps between them — the
                // inline plot never interpolates.
                pointRadius: 3,
                pointBackgroundColor: PLOT_COLORS[0],
                tension: 0,
                stepped: "before",
            }],
        },
        options: {
            responsive: true,
            maintainAspectRatio: false,
            animation: false,
            scales: {
                x: {
                    type: "linear",
                    min: Math.max(0, now - windowSizeSec),
                    max: Math.max(windowSizeSec, now),
                    ticks: { color: "#71717a", maxTicksLimit: 6, callback: (v: any) => `${Math.round(Number(v))}s` },
                    grid: { color: "#2a2b30" },
                },
                y: { ticks: { color: "#71717a" }, grid: { color: "#2a2b30" } },
            },
            plugins: {
                legend: { display: false },
                tooltip: { enabled: false },
            },
        },
    } as any);

    sigRow.classList.add("te-plotted");
    const tp: TracePlot = { chart, sigKey, data, lastTs: 0, sigRow };
    tracePlots.set(plotTr, tp);
    rebuildTracePlotKeys();
    startTracePlotLoop();

    // Seed with buffered history. Live points arriving while we await are
    // appended by onCanFrameBatch and are newer than anything returned here.
    try {
        const history = await invoke<Array<{ timestamp_ms: number; value: number }>>(
            "get_signal_history", { handle, messageId: msgId, signalName: sig.name, sinceMs: 0 },
        );
        if (tracePlots.get(plotTr) !== tp) return; // closed while awaiting
        const liveStart = tp.data[0]?.x ?? Infinity;
        const older = history
            .map(s => ({ ts: s.timestamp_ms, x: (s.timestamp_ms - appStartTime) / 1000, y: s.value }))
            .filter(p => p.x < liveStart);
        if (older.length > 0) {
            tp.data.unshift(...older.map(p => ({ x: p.x, y: p.y })));
            tp.lastTs = Math.max(tp.lastTs, older[older.length - 1].ts);
        }
        tp.chart.update("none");
    } catch {
        // History is best-effort; the plot still fills from live frames.
    }
}

// Build and insert the signal-detail expansion under a trace row. No-op when
// the row has no DBC match or is already expanded.
function expandTraceRow(tr: HTMLTableRowElement) {
    if (!tr.classList.contains("dbc-match") || tr.classList.contains("trace-row-expanded")) return;

    const trHandle = parseInt(tr.dataset.channelHandle ?? "0");
    const canId = parseInt(tr.dataset.canid ?? "0");
    const msg = dbcMessageFor(trHandle, canId, tr.dataset.ext === "1");
    if (!msg) return;
    // Interleaved [value, raw] pairs, index-aligned with msg.signals.
    const vals = traceRowSignals.get(tr) ?? [];

    const expandTr = document.createElement("tr");
    expandTr.dataset.expand = "1";
    // Identity for the inline signal plots (DBC message id, not the wire id).
    expandTr.dataset.handle = String(trHandle);
    expandTr.dataset.msgid = String(msg.id);
    const td = document.createElement("td");
    td.colSpan = visibleTraceCols().length;
    td.className = "trace-expand-cell";

    const hasEnums = msg.signals.some((s: DbcSignal) => (s.enum_values ?? []).length > 0);
    // The trailing filler column absorbs the leftover width so the table (and
    // with it row backgrounds and inline plot rows) spans the whole expansion.
    let html = '<table class="trace-expand-table"><thead><tr>'
        + '<th>Signal</th><th>Value</th><th>Min</th><th>Max</th><th>Unit</th>'
        + (hasEnums ? '<th>Name</th>' : '')
        + '<th class="te-fill"></th>'
        + '</tr></thead><tbody>';
    for (let i = 0; i < msg.signals.length; i++) {
        const sig = msg.signals[i];
        // Null pairs mark signals of a multiplexer group not active in this frame.
        const hasVal = 2 * i + 1 < vals.length && vals[2 * i] != null;
        // DBC message id, not the wire id — matches the keys written by
        // onCanFrameBatch (J1939 wire ids embed the sender's address).
        const key = plotKey(trHandle, msg.id, sig.name);
        const mn = signalMinValues.get(key);
        const mx = signalMaxValues.get(key);
        const fmt = (v: number | undefined) => v !== undefined ? formatSigValue(v, "") : "—";
        const rawTip = (v: number | undefined) => v !== undefined ? ` data-tip="Raw: ${physToRaw(sig, v)}"` : "";
        html += `<tr data-sig="${sig.name}">
        <td class="te-name" title="Click to plot">${sig.name}</td>
        <td class="te-val"${hasVal ? ` data-tip="Raw: ${vals[2 * i + 1]}"` : ""}>${hasVal ? formatSigValue(vals[2 * i]!, "") : "—"}</td>
        <td class="te-min"${rawTip(mn)}>${fmt(mn)}</td>
        <td class="te-max"${rawTip(mx)}>${fmt(mx)}</td>
        <td class="te-unit">${sig.unit || "—"}</td>`
            + (hasEnums ? `<td class="te-enum">${hasVal ? (enumLabelForRaw(sig, vals[2 * i + 1]!) || "—") : "—"}</td>` : "")
            + '<td class="te-fill"></td>'
            + `</tr>`;
    }
    html += '</tbody></table>';
    td.innerHTML = html;
    expandTr.appendChild(td);
    tr.after(expandTr);
    tr.classList.add("trace-row-expanded");
}

function collapseTraceRow(tr: HTMLTableRowElement, expandTr: HTMLTableRowElement) {
    // Destroy any inline signal plots living inside this expansion.
    for (const plotTr of [...tracePlots.keys()]) {
        if (expandTr.contains(plotTr)) destroyTracePlot(plotTr);
    }
    expandTr.remove();
    tr.classList.remove("trace-row-expanded");
}

function setupTrace() {
    rebuildTraceColumns();

    document.getElementById("btn-clear-trace")!.addEventListener("click", clearTrace);

    // Expand/collapse every decoded (DBC-matched) row currently in the table.
    document.getElementById("btn-trace-expand-all")!.addEventListener("click", () => {
        const tbody = document.getElementById("trace-tbody") as HTMLTableSectionElement;
        for (const tr of Array.from(tbody.rows) as HTMLTableRowElement[]) {
            if (tr.dataset.expand || tr.style.display === "none") continue;
            expandTraceRow(tr);
        }
    });
    document.getElementById("btn-trace-collapse-all")!.addEventListener("click", () => {
        const tbody = document.getElementById("trace-tbody") as HTMLTableSectionElement;
        for (const tr of Array.from(tbody.querySelectorAll<HTMLTableRowElement>("tr[data-expand]"))) {
            const parent = tr.previousElementSibling as HTMLTableRowElement | null;
            if (parent) collapseTraceRow(parent, tr);
            else { destroyTracePlot(tr); tr.remove(); }
        }
    });
    document.getElementById("btn-clear-filters")!.addEventListener("click", () => {
        clearAllFilters();
        scheduleAutoSave("trace filters cleared");
    });

    // ── Columns visibility button ─────────────────────────────────────────────
    document.getElementById("btn-trace-cols")!.addEventListener("click", (e) => {
        e.stopPropagation();
        if (ctxMenu) { ctxMenu.remove(); ctxMenu = null; return; }
        const btn = e.currentTarget as HTMLElement;
        const rect = btn.getBoundingClientRect();
        const menu = document.createElement("div");
        menu.className = "ctx-menu";
        menu.style.left = `${rect.left}px`;
        menu.style.top = `${rect.bottom + 4}px`;
        menu.addEventListener("click", ev => ev.stopPropagation());
        const showJ1939 = anyJ1939Channel();
        for (const def of TRACE_COL_DEFS) {
            if (!showJ1939 && J1939_COL_KEYS.has(def.key)) continue;
            const item = document.createElement("label");
            item.className = "ctx-col-item";
            const cb = document.createElement("input");
            cb.type = "checkbox";
            cb.checked = !traceColHidden.has(def.key);
            cb.addEventListener("change", () => {
                if (cb.checked) traceColHidden.delete(def.key);
                else traceColHidden.add(def.key);
                rebuildTraceColumns();
                scheduleAutoSave("trace column visibility toggled");
            });
            item.append(cb, " ", def.label);
            menu.appendChild(item);
        }
        const sep = document.createElement("div");
        sep.style.cssText = "border-top:1px solid var(--border);margin:4px 0";
        menu.appendChild(sep);
        const resetBtn = document.createElement("button");
        resetBtn.textContent = "Reset columns";
        resetBtn.className = "filter-ctrl-btn";
        resetBtn.style.cssText = "width:100%;margin:0";
        resetBtn.addEventListener("click", () => {
            traceColOrder = TRACE_COL_DEFS.map(d => d.key);
            traceColHidden = new Set();
            traceColWidths = {};
            rebuildTraceColumns();
            scheduleAutoSave("trace columns reset");
            ctxMenu?.remove(); ctxMenu = null;
        });
        menu.appendChild(resetBtn);
        ctxMenu = menu;
        document.body.appendChild(menu);
    });

    // ── Trace row expansion ───────────────────────────────────────────────────
    document.getElementById("trace-tbody")!.addEventListener("click", (e) => {
        const target = e.target as HTMLElement;

        // Click on a signal row inside an expansion: toggle its inline plot.
        const sigRow = target.closest<HTMLTableRowElement>(".trace-expand-table tbody tr");
        if (sigRow) {
            const expandTr = sigRow.closest<HTMLTableCellElement>("td.trace-expand-cell")?.parentElement as HTMLTableRowElement | null;
            if (expandTr?.dataset.expand && sigRow.dataset.sig) {
                const handle = parseInt(expandTr.dataset.handle ?? "0");
                const msgId = parseInt(expandTr.dataset.msgid ?? "0");
                const sig = channels.get(handle)?.dbc?.messages[msgId]?.signals
                    .find((s: DbcSignal) => s.name === sigRow.dataset.sig);
                if (sig) toggleTracePlot(sigRow, handle, msgId, sig);
            }
            return;
        }

        const tr = target.closest("tr") as HTMLTableRowElement | null;
        if (!tr || tr.dataset.expand || !tr.classList.contains("dbc-match")) return;

        const next = tr.nextElementSibling as HTMLTableRowElement | null;
        if (next?.dataset.expand) {
            collapseTraceRow(tr, next);
            return;
        }
        expandTraceRow(tr);
    });

    document.getElementById("btn-trace-overwrite")!.addEventListener("click", function () {
        const active = this.classList.toggle("active");
        traceMode = active ? "overwrite" : "append";
        clearTrace();
    });

    document.getElementById("input-trace-max")!.addEventListener("change", (e) => {
        traceMaxRows = parseInt((e.target as HTMLInputElement).value) || 100;
        while (traceLocalBuffer.length > traceMaxRows) traceLocalBuffer.pop();
        scheduleAutoSave("trace max rows changed");
    });

    // Cell content clipped by the fixed column layout (long payloads, message
    // names, …) is shown in full as a tooltip, on any trace cell. Capture phase
    // so data-tip is set before the document-level tooltip handler (bubble
    // phase) looks for it. Cells carrying a static data-tip of their own (the
    // PGN decimal value) are left alone; dynamically added clip-tips are marked
    // with data-clip-tip so they can be distinguished and removed again.
    document.getElementById("trace-container")!.addEventListener("mouseover", (e) => {
        const td = (e.target as HTMLElement).closest<HTMLTableCellElement>("td[data-col]");
        if (!td) return;
        if (td.dataset.tip !== undefined && td.dataset.clipTip === undefined) return;
        if (td.scrollWidth > td.clientWidth) {
            td.dataset.tip = td.textContent ?? "";
            td.dataset.clipTip = "1";
        } else {
            delete td.dataset.tip;
            delete td.dataset.clipTip;
        }
    }, true);
}

// ── Global pause ─────────────────────────────────────────────────────────────

function updatePauseViewBtn() {
    const btn = document.getElementById("btn-pause-view") as HTMLButtonElement;
    btn.textContent = viewPaused ? "Resume" : "Pause";
    btn.classList.toggle("running", viewPaused);
    // Pausing only makes sense while capture is live.
    btn.disabled = !appRunning;
}

// Rewrite every rendered sidebar row from the latest value maps. Used on resume
// so rows frozen during the pause catch up immediately.
function refreshSidebarValues() {
    for (const [key, valEl] of signalValueEls) {
        const v = signalLastValues.get(key);
        if (v == null) continue;
        valEl.textContent = sidebarValueText(key, v, signalLastRaw.get(key), signalUnits.get(key) ?? "");
        valEl.classList.remove("sig-value--empty");
    }
    for (const [key, rangeEl] of signalRangeEls) {
        const mn = signalMinValues.get(key);
        if (mn === undefined) continue;
        rangeEl.textContent = `↓${formatSigValue(mn, "")} ↑${formatSigValue(signalMaxValues.get(key)!, "")}`;
        rangeEl.classList.remove("sig-value--empty");
    }
}

function resumeFromPause() {
    // Sidebar values/min/max were frozen during the pause; snap them to current.
    sidebarSnapshot = null;
    refreshSidebarValues();

    // Flush pending trace updates
    const tbody = document.getElementById("trace-tbody") as HTMLTableSectionElement;
    if (traceMode === "overwrite") {
        for (const [key, entry] of tracePendingOverwrite) {
            const existing = traceRowEls.get(key);
            if (existing) {
                updateTraceRowEl(existing, entry);
            } else {
                const tr = buildTraceRow(entry);
                traceRowEls.set(key, tr);
                tbody.appendChild(tr);
            }
        }
        tracePendingOverwrite.clear();
        applyTraceSort();
    } else {
        // Re-render visible rows from the backend (newest first after refresh).
        loadTraceFrames().then(() => {
            destroyAllTracePlots();
            tbody.innerHTML = "";
            const frag = document.createDocumentFragment();
            let count = 0;
            for (const e of traceLocalBuffer) {
                if (count >= traceMaxRows) break;
                if (traceRowVisible(e.channelHandle, e.canId, e.data, e.direction, e.cycleTimeMs, e.dlc, e.messageName, e.j1939)) {
                    frag.appendChild(buildTraceRow(e));
                    count++;
                }
            }
            tbody.appendChild(frag);
            applyTraceSort();
        });
    }

    // Bring all plot panes up to date with accumulated data; clear any zoom.
    const now = (Date.now() - appStartTime) / 1000;
    for (const pane of plotPanes) {
        const seriesArray = [...pane.series.values()];
        for (let i = 0; i < seriesArray.length; i++) {
            seriesArray[i].frozenData = null;
            const ds = pane.chart.data.datasets[i] as any;
            if (ds) ds.data = seriesArray[i].data;
        }
        clearPaneZoom(pane);
        const xScale = (pane.chart.options.scales as any)["x"];
        xScale.min = Math.max(0, now - windowSizeSec);
        xScale.max = Math.max(windowSizeSec, now);
        pane.chart.update();
    }
    startScrollLoop();
}

// ── Utilities ─────────────────────────────────────────────────────────────────

type LogLevel = "debug" | "info" | "warn" | "error";
interface LogEntry { ts: string; text: string; level: LogLevel; }
const messageLog: LogEntry[] = [];
// Oldest entries are dropped past this point so a long session can't grow the
// log (array + DOM) without bound.
const MAX_LOG_ENTRIES = 300;

function updateWindowTitle() {
    const base = projectPath ? `Canvaz — ${projectPath}` : "Canvaz";
    getCurrentWindow().setTitle(projectDirty ? `${base} ●` : base);
}

// Escape a string for interpolation into innerHTML (element text or a
// double-quoted attribute) — device/driver-supplied names are not guaranteed
// free of HTML metacharacters.
function escapeHtml(s: string): string {
    return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

// Record an entry in the message log (array + panel DOM + console) and, unless
// suppressed, rotate it through the status bar. `ts` defaults to now; the
// rust-log relay passes the backend timestamp and writes its own (richer)
// console line, and suppresses the status bar when replaying the backlog.
function log(level: LogLevel, message: string, opts?: { ts?: string; toConsole?: boolean; toStatus?: boolean }) {
    // Debug messages are for development only — dropped entirely in release builds.
    if (level === "debug" && !import.meta.env.DEV) return;
    const entry: LogEntry = { ts: opts?.ts ?? new Date().toLocaleTimeString(), text: message, level };
    messageLog.push(entry);
    if (messageLog.length > MAX_LOG_ENTRIES) messageLog.shift();
    appendLogEntry(entry);
    if (level === "error") {
        document.getElementById("btn-show-log")?.classList.add("log-has-error");
    }
    if (opts?.toConsole !== false) {
        const fn = level === "error" ? console.error
            : level === "warn" ? console.warn
                : level === "info" ? console.info : console.debug;
        fn(message);
    }
    if (level !== "debug" && opts?.toStatus !== false) queueStatus(message, level);
}

// ── Status bar ────────────────────────────────────────────────────────────────
// Messages rotate through the status bar: each is shown for at least
// STATUS_MIN_MS (so a burst can't overwrite one instantly) and at most
// STATUS_MAX_MS, then slides out to the right — towards the full log, where
// every message already landed the moment it was logged.

const STATUS_MIN_MS = 1500;
const STATUS_MAX_MS = 4000;
const STATUS_EXIT_MS = 250; // keep in sync with the status-slide-out animation
const STATUS_MAX_QUEUE = 5;

const statusQueue: { text: string; level: LogLevel }[] = [];
let statusTimer: ReturnType<typeof setTimeout> | null = null;
let statusShownAt = 0;
let statusShowing = false;
let statusExiting = false;

function queueStatus(text: string, level: LogLevel) {
    statusQueue.push({ text, level });
    // A burst shouldn't back the bar up forever — dropped entries are in the log.
    while (statusQueue.length > STATUS_MAX_QUEUE) statusQueue.shift();
    if (!statusShowing) showNextStatus();
    // Current message already had its minimum time on screen — advance now.
    else if (!statusExiting && Date.now() - statusShownAt >= STATUS_MIN_MS) exitStatus();
}

function showNextStatus() {
    const bar = document.getElementById("status-bar")!;
    const text = document.getElementById("status-text")!;
    text.classList.remove("status-exit");
    const next = statusQueue.shift();
    if (!next) {
        statusShowing = false;
        text.textContent = "";
        bar.classList.remove("status-error", "status-warn");
        return;
    }
    statusShowing = true;
    statusShownAt = Date.now();
    text.textContent = next.text;
    bar.classList.toggle("status-error", next.level === "error");
    bar.classList.toggle("status-warn", next.level === "warn");
    if (statusTimer) clearTimeout(statusTimer);
    statusTimer = setTimeout(() => {
        if (statusQueue.length > 0) exitStatus();
        else statusTimer = setTimeout(exitStatus, STATUS_MAX_MS - STATUS_MIN_MS);
    }, STATUS_MIN_MS);
}

// Slide the current message out to the right, then show the next queued one.
function exitStatus() {
    if (statusExiting) return;
    statusExiting = true;
    if (statusTimer) { clearTimeout(statusTimer); statusTimer = null; }
    document.getElementById("status-text")!.classList.add("status-exit");
    setTimeout(() => { statusExiting = false; showNextStatus(); }, STATUS_EXIT_MS);
}

function appendLogEntry(entry: LogEntry) {
    const container = document.getElementById("log-entries");
    if (!container) return;
    const div = document.createElement("div");
    div.className = `log-entry log-${entry.level}`;
    const ts = document.createElement("span");
    ts.className = "log-ts";
    ts.textContent = entry.ts;
    const text = document.createElement("span");
    text.textContent = entry.text;
    div.appendChild(ts);
    div.appendChild(text);
    container.appendChild(div);
    while (container.childElementCount > MAX_LOG_ENTRIES) container.firstElementChild!.remove();
    container.scrollTop = container.scrollHeight;
}

// Surface uncaught frontend errors in the message log; the devtools console
// isn't visible in a packaged build.
window.addEventListener("error", (e) => {
    const loc = e.filename ? ` (${e.filename.split("/").pop()}:${e.lineno}:${e.colno})` : "";
    log("error", `Frontend error: ${e.message}${loc}`);
});
window.addEventListener("unhandledrejection", (e) => {
    const reason = e.reason instanceof Error ? (e.reason.stack ?? e.reason.message) : String(e.reason);
    log("error", `Unhandled promise rejection: ${reason}`);
});

// ── Boot ──────────────────────────────────────────────────────────────────────

window.addEventListener("DOMContentLoaded", async () => {
    // Pipe Rust log output into the message-log panel and the devtools
    // console (the console line also carries the module name; the panel just
    // shows the message). The backend keeps the full (ring-buffered) history,
    // so fetch that first — it includes everything logged before this page
    // existed (e.g. backend init during startup) — then follow live
    // "rust-log" events. Live events that arrive while the history fetch is
    // in flight are buffered and deduped against it by `seq`.
    interface RustLog { seq: number; ts: string; level: string; module: string; message: string; }
    // Rust timestamps are UTC HH:MM:SS.mmm; the panel shows local time.
    const rustTsToLocal = (utc: string) => {
        const m = utc.match(/^(\d+):(\d+):(\d+)\.(\d+)$/);
        if (!m) return utc;
        const d = new Date();
        d.setUTCHours(+m[1], +m[2], +m[3], +m[4]);
        return d.toLocaleTimeString();
    };
    // `live` distinguishes events from backlog replay: historical messages go
    // to the panel/console only, not through the status bar.
    const logRust = (e: RustLog, live: boolean) => {
        const line = `[rust ${e.ts} ${e.module}] ${e.message}`;
        if (e.level === "ERROR") console.error(line);
        else if (e.level === "WARN") console.warn(line);
        else if (e.level === "INFO") console.info(line);
        else console.log(line);
        const level: LogLevel =
            e.level === "ERROR" ? "error" :
                e.level === "WARN" ? "warn" :
                    e.level === "INFO" ? "info" : "debug";
        log(level, e.message, { ts: rustTsToLocal(e.ts), toConsole: false, toStatus: live });
    };
    let lastRustSeq = -1;
    let rustLogPending: RustLog[] | null = [];
    await listen<RustLog>("rust-log", (ev) => {
        if (rustLogPending) rustLogPending.push(ev.payload);
        else if (ev.payload.seq > lastRustSeq) { lastRustSeq = ev.payload.seq; logRust(ev.payload, true); }
    });
    const backlog = [
        ...await invoke<RustLog[]>("get_logs").catch(() => [] as RustLog[]),
        ...rustLogPending,
    ];
    rustLogPending = null;
    for (const e of backlog) {
        if (e.seq > lastRustSeq) { lastRustSeq = e.seq; logRust(e, false); }
    }

    // The Rust backend outlives a page reload, so any channels opened by the
    // previous load are still registered/open. Reset it before we rebuild state.
    await invoke("reset_backend").catch(e => log("debug", `Backend reset failed: ${e}`));

    // Tab switching
    document.querySelectorAll<HTMLButtonElement>(".tab-btn").forEach(btn => {
        btn.addEventListener("click", () => {
            document.querySelectorAll(".tab-btn").forEach(b => b.classList.remove("active"));
            document.querySelectorAll(".tab-content").forEach(c => c.classList.remove("active"));
            btn.classList.add("active");
            document.getElementById(`tab-${btn.dataset.tab}`)!.classList.add("active");
            plotTabActive = btn.dataset.tab === "plot";
            traceTabActive = btn.dataset.tab === "trace";
            if (plotTabActive && appRunning && !viewPaused) startScrollLoop();
            if (traceTabActive && appRunning) {
                loadTraceFrames().then(() => applyTraceFilter());
            }
        });
    });

    // DBC tree interactions (delegated drag / double-click)
    setupDbcTree();

    // DBC filter — debounced so fast typing doesn't rebuild the tree per keystroke.
    let searchDebounce: ReturnType<typeof setTimeout> | null = null;
    document.getElementById("signal-search")!.addEventListener("input", (e) => {
        const value = (e.target as HTMLInputElement).value;
        if (searchDebounce) clearTimeout(searchDebounce);
        searchDebounce = setTimeout(() => renderDbcTree(value), 120);
    });

    // Expand / collapse all DBC message groups
    document.getElementById("btn-expand-all")!.addEventListener("click", () => {
        document.querySelectorAll<HTMLDetailsElement>("#dbc-tree details").forEach(d => { d.open = true; });
    });
    document.getElementById("btn-collapse-all")!.addEventListener("click", () => {
        document.querySelectorAll<HTMLDetailsElement>("#dbc-tree details").forEach(d => { d.open = false; });
    });

    // Channel dialog
    const chanDialog = document.getElementById("dialog-channel") as HTMLDialogElement;
    document.getElementById("btn-add-channel")!.addEventListener("click", async () => {
        if (!await confirmAndStop("Stop live capture to add a channel?")) return;
        openChannelDialog("add");
    });
    document.getElementById("btn-reload-backends")!.addEventListener("click", async () => {
        if (!await confirmAndStop("Stop live capture to reload CAN backends?")) return;
        let remapped: { old_handle: number; new_handle: number; backend: string }[];
        try {
            remapped = await invoke<{ old_handle: number; new_handle: number; backend: string }[]>("reload_backends");
        } catch (e) {
            log("error", `Backend reload failed: ${e}`);
            return;
        }

        // Apply old→new handle remapping. The resolved backend can differ from
        // the previous one (the name is searched in every backend), so take it
        // from the remap entry. Channels not in the remapping failed to
        // re-register (hardware absent) and become ghosts.
        const handleMap = new Map(remapped.map(r => [r.old_handle, r]));
        const oldEntries = [...channels.entries()];
        channels.clear();
        for (const [oldHandle, ch] of oldEntries) {
            const remap = handleMap.get(oldHandle);
            if (remap !== undefined) {
                ch.config.backend = remap.backend;
                channels.set(remap.new_handle, { ...ch, info: { ...ch.info, backend: remap.backend }, open: false });
            } else {
                ghostChannels.push({ config: ch.config, error: "Not found after backend reload" });
            }
        }

        // Promote ghost channels whose hardware is now available. create_channel
        // searches every backend for the name, so a guessed backend still works.
        const recovered: GhostChannel[] = [];
        for (const ghost of [...ghostChannels]) {
            const res = await registerChannel(ghost.config);
            if (res.handle !== undefined) recovered.push(ghost);
            else ghost.error = res.error!; // stays as ghost
        }
        for (const g of recovered) ghostChannels.splice(ghostChannels.indexOf(g), 1);
        renderChannelList();
    });
    document.getElementById("btn-channel-cancel")!.addEventListener("click", () => chanDialog.close());
    document.getElementById("form-channel")!.addEventListener("submit", async (e) => { e.preventDefault(); await applyChannelDialog(); });

    // Bitrate custom field toggle
    document.getElementById("select-bitrate")!.addEventListener("change", (e) => {
        const v = (e.target as HTMLSelectElement).value;
        const customInput = document.getElementById("input-bitrate-custom") as HTMLInputElement;
        customInput.style.display = v === "custom" ? "" : "none";
        if (v !== "custom") customInput.value = "";
    });

    // DBC browse in dialog
    document.getElementById("btn-browse-dbc")!.addEventListener("click", async () => {
        const path = await dialogOpen({ filters: [{ name: "DBC Files", extensions: ["dbc"] }], multiple: false });
        if (!path || Array.isArray(path)) return;
        dialogPendingDbc = path;
        setDbcLabel(path);
    });
    document.getElementById("btn-clear-dbc")!.addEventListener("click", () => {
        dialogPendingDbc = null;
        setDbcLabel(null);
    });

    document.getElementById("btn-add-raw-frame")!.addEventListener("click", addRawFrame);

    // Play / Stop
    document.getElementById("btn-app-run")!.addEventListener("click", async () => {
        if (appRunning) await stopApp(); else await startApp();
    });

    document.getElementById("btn-pause-view")!.addEventListener("click", () => {
        if (!appRunning) return; // button is disabled while stopped; belt & braces
        viewPaused = !viewPaused;
        updatePauseViewBtn();
        if (viewPaused) snapshotPlotPanes();
        else resumeFromPause();
    });

    // Log panel
    const logPanel = document.getElementById("log-panel")!;
    const pinLogBtn = document.getElementById("btn-pin-log")!;
    let logPinned = false;
    // Pinned, the panel docks into the layout as a right-hand pane and stays
    // put; unpinned it is the transient overlay that closes on outside click.
    // The element always lives in #layout — position:fixed lifts it out of the
    // flex flow while unpinned, so only the class needs toggling.
    const setLogPinned = (pinned: boolean) => {
        logPinned = pinned;
        logPanel.classList.toggle("pinned", pinned);
        pinLogBtn.classList.toggle("active", pinned);
        pinLogBtn.title = pinned ? "Unpin log panel" : "Pin log panel to the layout";
    };
    pinLogBtn.addEventListener("click", () => {
        setLogPinned(!logPinned);
        preferences.logPinned = logPinned;
        savePreferences();
    });
    document.getElementById("btn-show-log")!.addEventListener("click", () => {
        logPanel.hidden = !logPanel.hidden;
        if (!logPanel.hidden) {
            document.getElementById("btn-show-log")!.classList.remove("log-has-error");
            const entries = document.getElementById("log-entries")!;
            entries.scrollTop = entries.scrollHeight;
        }
    });
    document.getElementById("btn-close-log")!.addEventListener("click", () => {
        logPanel.hidden = true;
    });
    document.getElementById("btn-clear-log")!.addEventListener("click", () => {
        messageLog.length = 0;
        document.getElementById("log-entries")!.innerHTML = "";
    });
    document.addEventListener("pointerdown", (e) => {
        if (logPanel.hidden || logPinned) return;
        if (!logPanel.contains(e.target as Node) && e.target !== document.getElementById("btn-show-log")) {
            logPanel.hidden = true;
        }
    });
    document.addEventListener("keydown", (e) => {
        if (e.key === "Escape" && !logPinned) logPanel.hidden = true;
    });

    // Menu bar
    setupMenuBar();

    // Preferences (per-user, persisted across restarts)
    await loadPreferences();
    if (preferences.logPinned) {
        setLogPinned(true);
        logPanel.hidden = false;
    }

    // Check for a newer release. Only nag on real release builds; dev builds can
    // still check manually via About → Check for Updates. Fire-and-forget so a
    // slow or offline network never blocks startup.
    currentVersion = await invoke<string>("get_version").catch(() => "");
    if (isReleaseBuild(currentVersion)) checkForUpdates(false);

    applySidebarWidth();
    setupSidebarResize();
    setupWindowSize();

    // Drop zones
    setupDropZone();
    setupSimDrop();

    // Trace
    setupTrace();

    // Events — the backend batches frames and emits one event per ~33 ms tick.
    await listen<CanFrameEvent[]>("can-frame-batch", (event) => onCanFrameBatch(event.payload));

    // Channel RX/TX failures surfaced by the backend (device unplugged, bus
    // errors); fatal ones badge the channel and trigger background recovery.
    await listen<ChannelErrorEvent>("channel-error", (event) => { onChannelError(event.payload); });

    // Sudo password request from the Rust backend — show dialog once, cache in Rust.
    await listen("request-admin-password", async () => {
        const pw = await promptSudoPassword();
        await invoke("provide_admin_password", { password: pw ?? null }).catch(() => { });
    });

    // Resolve paths
    const dir = await invoke<string>("get_app_data_dir");
    const sess = `${dir}/last-session.canvaz`;
    lastProjectIndexPath = `${dir}/last-project.txt`;

    const lastPath = await invoke<string>("read_text_file", { path: lastProjectIndexPath }).catch(() => null);
    const lastPathExists = lastPath ? await invoke<boolean>("file_exists", { path: lastPath }) : false;

    let sessionProject: Project | null = null;
    try { sessionProject = await invoke<Project>("load_project", { path: sess }); } catch { }

    if (sessionProject) {
        // Apply session without triggering auto-save or dirty tracking during restore
        await applyProject(sessionProject);
        sessionFilePath = sess;

        if (lastPathExists) {
            projectPath = lastPath!;
            // Mark dirty only if session differs from the saved project file
            const [sessionContent, projectContent] = await Promise.all([
                invoke<string>("read_text_file", { path: sess }).catch(() => ""),
                invoke<string>("read_text_file", { path: lastPath! }).catch(() => ""),
            ]);
            projectDirty = sessionContent !== projectContent;
        } else {
            projectDirty = false;
        }
        updateWindowTitle();
        log("info", "Session restored");
    } else {
        sessionFilePath = sess;
        // No previous session — start fresh with one empty pane
        createPlotPane();
        refreshChannelList();
        renderDbcTree();
    }

    // Auto-start only when at least one channel has been configured.
    if (channels.size > 0) {
        await startApp();
    }
    // Otherwise the app stays in stopped state; user adds channels then presses Start.
});
