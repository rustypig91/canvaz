import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as dialogOpen, save as dialogSave } from "@tauri-apps/plugin-dialog";
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
  CategoryScale,
} from "chart.js";
import ZoomPlugin from "chartjs-plugin-zoom";

Chart.register(LineController, LineElement, PointElement, LinearScale, Legend, Title, Tooltip, Filler, CategoryScale, ZoomPlugin);

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
}

interface DbcMessage {
  id: number;
  name: string;
  dlc: number;
  signals: DbcSignal[];
}

interface ParsedDbc {
  path: string;
  messages: DbcMessage[];
}

interface SignalValueEvent {
  channel_id: string;
  signal_name: string;
  message_name: string;
  value: number;
  unit: string;
  timestamp_ms: number;
}

interface CanFrameEvent {
  channel_id: string;
  can_id: number;
  is_extended: boolean;
  dlc: number;
  data: number[];
  timestamp_ms: number;
  direction: "rx" | "tx";
}

interface PlotSignalEntry { signal_name: string; channel: string; }
interface PlotPaneConfig  { signals: PlotSignalEntry[]; }
interface ChannelInfo     { id: string; backend: string; name: string; }
interface ChannelConfig   { name: string; backend: string; dbc_path: string | null; bitrate?: number | null; }
interface SimulateEntry   { signal_name: string; channel: string; value: number; period_ms: number; }

interface Project {
  version: number;
  channels: ChannelConfig[];
  plot_panes: PlotPaneConfig[];
  simulate_signals: SimulateEntry[];
}

// ── Plot pane state ───────────────────────────────────────────────────────────

const PLOT_COLORS = [
  "#3b82f6","#22c55e","#f59e0b","#ef4444",
  "#8b5cf6","#06b6d4","#f97316","#ec4899",
];

interface PlotSeries {
  signalName: string;
  messageName: string;
  unit: string;
  color: string;
  channel: string;
  timestamps: number[];
  labels: string[];
  values: number[];
  lastValue: number | null;
}

interface PlotPane {
  id: string;
  el: HTMLElement;
  chart: Chart;
  series: Map<string, PlotSeries>;   // key: channel::signalName
  interpolate: boolean;
  showPoints: boolean;
  hoveredDatasetIndex: number | null;
  zoomed: boolean;
}

const plotPanes: PlotPane[] = [];
let paneCounter = 0;
let plotPaused = false;
let maxTimeSecs: number | null = null; // null = unlimited

let appRunning = true;
let appStartTime = Date.now();

interface SignalSample { ts: number; value: number; unit: string; }
const signalHistory = new Map<string, SignalSample[]>();

function plotKey(channel: string, signalName: string) {
  return `${channel}::${signalName}`;
}

function decodeSignal(data: number[], sig: DbcSignal): number {
  const { start_bit, length, little_endian, signed, factor, offset: sigOffset } = sig;
  let raw = 0n;
  const len = BigInt(length);
  if (little_endian) {
    for (let i = 0; i < length; i++) {
      const byteIdx = ((start_bit + i) / 8) | 0;
      const bitInByte = (start_bit + i) % 8;
      if (byteIdx < data.length) raw |= BigInt((data[byteIdx] >> bitInByte) & 1) << BigInt(i);
    }
  } else {
    let bitPos = start_bit;
    for (let i = 0; i < length; i++) {
      const byteIdx = (bitPos / 8) | 0;
      const bitInByte = bitPos % 8;
      if (byteIdx < data.length) raw |= BigInt((data[byteIdx] >> bitInByte) & 1) << BigInt(length - 1 - i);
      if (bitPos % 8 === 0) bitPos += 15; else bitPos -= 1;
    }
  }
  let physical: number;
  if (signed && length > 0 && (raw & (1n << (len - 1n)))) {
    physical = Number(BigInt.asIntN(64, raw | (~((1n << len) - 1n))));
  } else {
    physical = Number(raw);
  }
  return physical * factor + sigOffset;
}

function formatSigValue(value: number, unit: string): string {
  const abs = Math.abs(value);
  const s = abs >= 10000 ? value.toFixed(0)
           : abs >= 100  ? value.toFixed(1)
           : abs >= 10   ? value.toFixed(2)
           : value.toFixed(3);
  return unit ? `${s} ${unit}` : s;
}

// ── Pane lifecycle ────────────────────────────────────────────────────────────

function updatePaneTitle(pane: PlotPane) {
  const names = [...pane.series.values()].map(s => s.signalName);
  const title = pane.el.querySelector<HTMLElement>(".pane-title")!;
  title.textContent = names.length ? names.join(", ") : `Plot ${pane.id.replace("pane-", "")}`;
}

function removeSigFromPane(pane: PlotPane, key: string) {
  if (!pane.series.delete(key)) return;
  syncDatasets(pane);
  updatePaneTitle(pane);
  (pane.chart.data as { labels: string[] }).labels = [];
  updateSignalHighlights();
  scheduleAutoSave();
}

function createPlotPane(): PlotPane {
  const id = `pane-${++paneCounter}`;

  // Allocate pane object first so legend callbacks can close over it
  const pane: PlotPane = { id, el: null!, chart: null!, series: new Map(), interpolate: false, showPoints: false, hoveredDatasetIndex: null, zoomed: false };

  const el = document.createElement("div");
  el.className = "plot-pane";
  el.id = id;
  el.dataset.paneId = id;
  el.innerHTML = `
    <div class="pane-header">
      <span class="pane-title">Plot ${paneCounter}</span>
      <button class="btn-reset-zoom pane-btn" title="Reset zoom" style="display:none">⟲</button>
      <button class="btn-show-points pane-btn" title="Show data points: off">•</button>
      <button class="btn-interp pane-btn" title="Interpolation: off">∿</button>
      <button class="btn-close-pane" title="Close plot">×</button>
    </div>
    <div class="pane-canvas-wrap">
      <canvas></canvas>
    </div>
  `;
  el.querySelector(".btn-close-pane")!.addEventListener("click", () => closePlotPane(id));
  el.querySelector<HTMLButtonElement>(".btn-show-points")!.addEventListener("click", (e) => {
    pane.showPoints = !pane.showPoints;
    const btn = e.currentTarget as HTMLButtonElement;
    btn.classList.toggle("active", pane.showPoints);
    btn.title = `Show data points: ${pane.showPoints ? "on" : "off"}`;
    syncDatasets(pane);
  });
  el.querySelector<HTMLButtonElement>(".btn-interp")!.addEventListener("click", (e) => {
    pane.interpolate = !pane.interpolate;
    const btn = e.currentTarget as HTMLButtonElement;
    btn.classList.toggle("active", pane.interpolate);
    btn.title = `Interpolation: ${pane.interpolate ? "on" : "off"}`;
    syncDatasets(pane);
  });
  const resetZoomBtn = el.querySelector<HTMLButtonElement>(".btn-reset-zoom")!;
  resetZoomBtn.addEventListener("click", () => {
    pane.chart.resetZoom();
    pane.zoomed = false;
    resetZoomBtn.style.display = "none";
  });

  const canvas = el.querySelector<HTMLCanvasElement>("canvas")!;
  const chart = new Chart(canvas, {
    type: "line",
    data: { labels: [], datasets: [] },
    options: {
      animation: false,
      responsive: true,
      maintainAspectRatio: false,
      interaction: { mode: "index", intersect: false },
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
            onZoomComplete: () => {
              pane.zoomed = true;
              resetZoomBtn.style.display = "";
              if (!plotPaused) {
                plotPaused = true;
                const btn = document.getElementById("btn-pause-plot")!;
                btn.textContent = "Resume";
                btn.classList.add("running");
              }
            },
          },
        },
      },
      scales: {
        x: { ticks: { color: "#71717a", maxTicksLimit: 8, maxRotation: 0 }, grid: { color: "#2a2b30" } },
        y: { ticks: { color: "#71717a" }, grid: { color: "#2a2b30" } },
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
  scheduleAutoSave();
}

// ── Signal → pane ─────────────────────────────────────────────────────────────

function addSignalToPane(pane: PlotPane, channel: string, sig: DbcSignal) {
  const key = plotKey(channel, sig.name);
  if (pane.series.has(key)) return;
  const color = PLOT_COLORS[pane.series.size % PLOT_COLORS.length];
  const series: PlotSeries = {
    signalName: sig.name, messageName: sig.message_name, unit: sig.unit,
    color, channel, timestamps: [], labels: [], values: [], lastValue: null,
  };

  // Pre-populate from global history
  const now = Date.now();
  const hist = signalHistory.get(key) ?? [];
  for (const sample of hist) {
    if (maxTimeSecs !== null && (now - sample.ts) > maxTimeSecs * 1000) continue;
    series.timestamps.push(sample.ts);
    series.labels.push(fmtPlotLabel(sample.ts));
    series.values.push(sample.value);
  }
  if (series.values.length > 0) series.lastValue = series.values[series.values.length - 1];

  pane.series.set(key, series);

  // Extend chart labels if this series has more data than the current chart
  const chartLabels = (pane.chart.data as { labels: string[] }).labels as string[];
  if (series.labels.length > chartLabels.length) {
    (pane.chart.data as { labels: string[] }).labels = series.labels;
  }

  syncDatasets(pane);
  updatePaneTitle(pane);
  updateSignalHighlights();
  scheduleAutoSave();
}

function syncDatasets(pane: PlotPane) {
  const tension = pane.interpolate ? 0.4 : 0;
  pane.chart.data.datasets = [...pane.series.values()].map((s, i) => {
    const hovered = pane.hoveredDatasetIndex === i;
    const showDot = pane.showPoints || hovered;
    return {
      label: s.signalName,
      data: s.values,
      borderColor: s.color,
      pointBackgroundColor: s.color,
      backgroundColor: "transparent",
      borderWidth: hovered ? 2.5 : 1.5,
      pointRadius: showDot ? 3 : 0,
      pointHoverRadius: hovered ? 5 : 3,
      tension,
    };
  });
  pane.chart.update("none");
}

// ── Signal value events ───────────────────────────────────────────────────────

function onSignalValue(ev: SignalValueEvent) {
  if (!appRunning) return;
  const key = plotKey(ev.channel_id, ev.signal_name);

  // Store in global history
  let hist = signalHistory.get(key);
  if (!hist) { hist = []; signalHistory.set(key, hist); }
  hist.push({ ts: ev.timestamp_ms, value: ev.value, unit: ev.unit });
  if (maxTimeSecs !== null) {
    const cutoff = ev.timestamp_ms - maxTimeSecs * 1000;
    while (hist.length > 0 && hist[0].ts < cutoff) hist.shift();
  }

  // Update sidebar value + min/max
  signalLastValues.set(key, ev.value);
  const prevMin = signalMinValues.get(key);
  const prevMax = signalMaxValues.get(key);
  if (prevMin === undefined || ev.value < prevMin) signalMinValues.set(key, ev.value);
  if (prevMax === undefined || ev.value > prevMax) signalMaxValues.set(key, ev.value);

  const valEl = signalValueEls.get(key);
  if (valEl) {
    valEl.textContent = formatSigValue(ev.value, ev.unit);
    valEl.classList.remove("sig-value--empty");
  }
  const rangeEl = signalRangeEls.get(key);
  if (rangeEl) {
    const mn = signalMinValues.get(key)!;
    const mx = signalMaxValues.get(key)!;
    rangeEl.textContent = `↓${formatSigValue(mn, "")} ↑${formatSigValue(mx, "")}`;
    rangeEl.classList.remove("sig-value--empty");
  }

  if (plotPaused) return;
  const ts = fmtPlotLabel(ev.timestamp_ms);

  for (const pane of plotPanes) {
    const series = pane.series.get(key);
    if (!series) continue;
    series.timestamps.push(ev.timestamp_ms);
    series.labels.push(ts);
    series.values.push(ev.value);
    series.lastValue = ev.value;
    if (maxTimeSecs !== null) {
      const cutoff = ev.timestamp_ms - maxTimeSecs * 1000;
      while (series.timestamps.length > 0 && series.timestamps[0] < cutoff) {
        series.timestamps.shift();
        series.labels.shift();
        series.values.shift();
      }
    }
    (pane.chart.data as { labels: string[] }).labels = series.labels;
    pane.chart.update("none");
  }
}

// ── Signal highlights in DBC tree ─────────────────────────────────────────────

function updateSignalHighlights() {
  const plotted = new Set<string>();
  for (const pane of plotPanes)
    for (const key of pane.series.keys()) plotted.add(key);

  const simulated = new Set<string>();
  for (const entry of simEntries.values()) {
    if (entry.kind === "message") {
      for (const s of entry.signals) simulated.add(plotKey(entry.channel, s.def.name));
    }
  }

  document.querySelectorAll<HTMLElement>(".signal-row").forEach(row => {
    const key = plotKey(row.dataset.channel ?? "", row.dataset.signal ?? "");
    row.classList.toggle("in-plot", plotted.has(key));
    row.classList.toggle("in-sim", simulated.has(key));
  });
}

// ── DBC tree rendering ────────────────────────────────────────────────────────

function renderDbcTree(filter = "") {
  const tree = document.getElementById("dbc-tree")!;
  tree.innerHTML = "";
  signalValueEls.clear();
  signalRangeEls.clear();

  const dbc = selectedChannel ? dbcByChannel.get(selectedChannel) : null;
  if (!dbc) {
    tree.innerHTML = `<div style="padding:8px 12px;color:var(--text-muted);font-size:11px">${
      selectedChannel ? "No DBC loaded for this channel" : "Select a channel"
    }</div>`;
    return;
  }

  const lc = filter.toLowerCase();

  for (const msg of dbc.messages) {
    const visibleSignals = msg.signals.filter(s =>
      !lc || s.name.toLowerCase().includes(lc) || msg.name.toLowerCase().includes(lc)
    );
    if (!visibleSignals.length) continue;

    const details = document.createElement("details");
    details.className = "msg-group";
    if (filter) details.open = true;

    const summary = document.createElement("summary");
    summary.innerHTML = `${msg.name}<span class="msg-id-badge">0x${msg.id.toString(16).toUpperCase().padStart(3,"0")}</span>`;
    details.appendChild(summary);

    for (const sig of visibleSignals) {
      const row = document.createElement("div");
      row.className = "signal-row";
      row.dataset.signal = sig.name;
      row.dataset.channel = selectedChannel!;
      const key = plotKey(selectedChannel!, sig.name);
      const lastVal = signalLastValues.get(key);
      const valText = lastVal != null ? formatSigValue(lastVal, sig.unit) : (sig.unit || "");
      const mn = signalMinValues.get(key);
      const mx = signalMaxValues.get(key);
      const rangeText = mn !== undefined ? `↓${formatSigValue(mn, "")} ↑${formatSigValue(mx!, "")}` : "↓— ↑—";
      row.innerHTML = `
        <span class="sig-name">${sig.name}</span>
        <span class="sig-value${lastVal == null ? " sig-value--empty" : ""}">${valText}</span>
        <span class="sig-range${mn === undefined ? " sig-value--empty" : ""}">${rangeText}</span>`;
      signalValueEls.set(key, row.querySelector<HTMLElement>(".sig-value")!);
      signalRangeEls.set(key, row.querySelector<HTMLElement>(".sig-range")!);

      // Drag to plot
      row.setAttribute("draggable", "true");
      row.addEventListener("dragstart", (e) => {
        if (!selectedChannel) { e.preventDefault(); return; }
        const payload: DragSignal = {
          channel: selectedChannel,
          signalName: sig.name,
          messageName: sig.message_name,
          unit: sig.unit,
          sig,
        };
        e.dataTransfer!.setData("application/can-signal", JSON.stringify(payload));
        e.dataTransfer!.effectAllowed = "copy";
      });

      // Double-click: add to first pane (create one if needed)
      row.addEventListener("dblclick", () => {
        const activeTab = document.querySelector(".tab-btn.active")?.getAttribute("data-tab");
        if (activeTab === "plot") {
          const pane = plotPanes[0] ?? createPlotPane();
          addSignalToPane(pane, selectedChannel!, sig);
        } else if (activeTab === "simulate") {
          addSimSignal(selectedChannel!, sig);
        }
      });

      details.appendChild(row);
    }
    tree.appendChild(details);
  }

  // Refresh highlights after tree rebuild
  updateSignalHighlights();
}

// ── Drag & drop ───────────────────────────────────────────────────────────────

interface DragSignal {
  channel: string;
  signalName: string;
  messageName: string;
  unit: string;
  sig: DbcSignal;
}

function parseDragSignal(e: DragEvent): DragSignal | null {
  try { return JSON.parse(e.dataTransfer?.getData("application/can-signal") ?? "null"); }
  catch { return null; }
}

function setupPaneDrop(el: HTMLElement, pane: PlotPane) {
  let dragDepth = 0;
  el.addEventListener("dragenter", (e) => {
    if (!e.dataTransfer?.types.includes("application/can-signal")) return;
    e.preventDefault();
    if (++dragDepth === 1) el.classList.add("drag-over");
  });
  el.addEventListener("dragover", (e) => {
    if (!e.dataTransfer?.types.includes("application/can-signal")) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "copy";
  });
  el.addEventListener("dragleave", () => {
    if (--dragDepth === 0) el.classList.remove("drag-over");
  });
  el.addEventListener("drop", (e) => {
    e.preventDefault();
    dragDepth = 0;
    el.classList.remove("drag-over");
    const data = parseDragSignal(e);
    if (data) addSignalToPane(pane, data.channel, data.sig);
  });
}

function setupDropZone() {
  const zone = document.getElementById("drop-zone-new")!;
  let dragDepth = 0;
  zone.addEventListener("dragenter", (e) => {
    if (!e.dataTransfer?.types.includes("application/can-signal")) return;
    e.preventDefault();
    if (++dragDepth === 1) zone.classList.add("drag-over");
  });
  zone.addEventListener("dragover", (e) => {
    if (!e.dataTransfer?.types.includes("application/can-signal")) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "copy";
  });
  zone.addEventListener("dragleave", () => {
    if (--dragDepth === 0) zone.classList.remove("drag-over");
  });
  zone.addEventListener("drop", (e) => {
    e.preventDefault();
    dragDepth = 0;
    zone.classList.remove("drag-over");
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
    if (!e.dataTransfer?.types.includes("application/can-signal")) return;
    e.preventDefault();
    if (++dragDepth === 1) zone.classList.add("drag-over");
  });
  zone.addEventListener("dragover", (e) => {
    if (!e.dataTransfer?.types.includes("application/can-signal")) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "copy";
  });
  zone.addEventListener("dragleave", () => {
    if (--dragDepth === 0) zone.classList.remove("drag-over");
  });
  zone.addEventListener("drop", (e) => {
    e.preventDefault();
    dragDepth = 0;
    zone.classList.remove("drag-over");
    const data = parseDragSignal(e);
    if (data) addSimSignal(data.channel, data.sig);
  });
}

// ── App state ─────────────────────────────────────────────────────────────────

const dbcByChannel = new Map<string, ParsedDbc>();       // channel_id → DBC
const channelBitrates = new Map<string, number | null>(); // channel_id → bitrate
const interfaceBackends = new Map<string, string>();      // from list_can_interfaces: name → backend

function channelDisplayName(id: string) { return id.includes(':') ? id.split(':')[1] : id; }
function channelBackend(id: string)     { return id.includes(':') ? id.split(':')[0] : ""; }
const signalLastValues = new Map<string, number>();
const signalMinValues  = new Map<string, number>();
const signalMaxValues  = new Map<string, number>();
const signalValueEls   = new Map<string, HTMLElement>();
const signalRangeEls   = new Map<string, HTMLElement>();
let selectedChannel: string | null = null;

// ── Auto-save / session restore ───────────────────────────────────────────────

let sessionFilePath: string | null = null;
let autoSaveTimer: ReturnType<typeof setTimeout> | null = null;

function scheduleAutoSave() {
  if (!sessionFilePath) return;
  if (autoSaveTimer) clearTimeout(autoSaveTimer);
  autoSaveTimer = setTimeout(async () => {
    try { await invoke("save_project", { path: sessionFilePath, project: buildProject() }); }
    catch { /* silent — auto-save failures should not interrupt the user */ }
  }, 1000);
}
let openChannels: string[] = [];
let projectPath: string | null = null;

// ── Channel dialog ────────────────────────────────────────────────────────────

type DialogMode = "add" | "edit";
let dialogMode: DialogMode = "add";
let dialogEditTarget: string | null = null;
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

function setBitrateInDialog(bitrate: number | null, isVcan: boolean) {
  const sel = document.getElementById("select-bitrate") as HTMLSelectElement;
  const custom = document.getElementById("input-bitrate-custom") as HTMLInputElement;
  if (isVcan) {
    sel.value = "vcan";
    custom.style.display = "none";
  } else if (bitrate != null && ["125000","250000","500000","1000000"].includes(String(bitrate))) {
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

async function openChannelDialog(mode: DialogMode, channelName?: string) {
  dialogMode = mode;
  dialogEditTarget = channelName ?? null;

  const dialog = document.getElementById("dialog-channel") as HTMLDialogElement;
  const title = document.getElementById("dialog-channel-title")!;
  const applyBtn = document.getElementById("btn-channel-apply")!;
  const ifaceRow = document.getElementById("row-iface")!;
  const sel = document.getElementById("select-iface") as HTMLSelectElement;

  if (mode === "add") {
    title.textContent = "Add CAN Channel";
    applyBtn.textContent = "Add";
    ifaceRow.style.display = "";
    (document.getElementById("input-iface-custom") as HTMLInputElement).value = "";
    sel.innerHTML = "";
    dialogPendingDbc = null;
    setDbcLabel(null);
    setBitrateInDialog(500000, false);
    const ifaces = await invoke<ChannelInfo[]>("list_can_interfaces").catch(() => [] as ChannelInfo[]);
    interfaceBackends.clear();
    for (const i of ifaces) interfaceBackends.set(i.name, i.backend);

    // Group interfaces by backend into <optgroup> elements
    const byBackend = new Map<string, string[]>();
    for (const i of ifaces) {
      const group = byBackend.get(i.backend) ?? [];
      group.push(i.name);
      byBackend.set(i.backend, group);
    }
    sel.innerHTML = [...byBackend.entries()]
      .map(([backend, names]) =>
        `<optgroup label="${backend}">${names.map(n => `<option value="${n}">${n}</option>`).join("")}</optgroup>`
      ).join("");

    // Auto-detect vcan from first item
    if (ifaces[0]?.name.startsWith("vcan")) setBitrateInDialog(null, true);
  } else {
    const id = channelName!;
    const displayName = channelDisplayName(id);
    title.textContent = `Channel: ${displayName}`;
    applyBtn.textContent = "Apply";
    ifaceRow.style.display = "none";
    const dbc = dbcByChannel.get(id);
    dialogPendingDbc = dbc?.path ?? null;
    setDbcLabel(dialogPendingDbc);
    setBitrateInDialog(channelBitrates.get(id) ?? null, displayName.startsWith("vcan"));
    selectChannel(id);
  }

  dialog.showModal();
}

// ── Sudo password ─────────────────────────────────────────────────────────────

function promptSudoPassword(): Promise<string | null> {
  return new Promise((resolve) => {
    const dialog = document.getElementById("dialog-sudo") as HTMLDialogElement;
    const input  = document.getElementById("input-sudo-pw") as HTMLInputElement;
    const form   = document.getElementById("form-sudo")!;
    const cancel = document.getElementById("btn-sudo-cancel")!;

    input.value = "";
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

// Open a channel. If root is required the Rust side emits "request-sudo-password",
// the global listener shows the dialog, and open_channel unblocks automatically.
async function openChannel(
  backend: string,
  name: string,
  bitrate: number | null,
): Promise<ChannelInfo | null> {
  console.log(`Opening channel: backend=${backend}, name=${name}, bitrate=${bitrate}`);
  try {
    return await invoke<ChannelInfo>("open_channel", { backendName: backend, channelName: name, bitrate: bitrate });
  } catch (e) {
    const msg = String(e);
    if (msg === "Sudo authentication cancelled") {
      setStatus("Cancelled — sudo password required.");
    } else {
      setStatus(`Channel error: ${msg}`);
    }
    return null;
  }
}

async function applyChannelDialog() {
  const dialog = document.getElementById("dialog-channel") as HTMLDialogElement;
  const bitrate = getBitrateFromDialog();

  if (dialogMode === "add") {
    const custom = (document.getElementById("input-iface-custom") as HTMLInputElement).value.trim();
    const name = custom || (document.getElementById("select-iface") as HTMLSelectElement).value;
    if (!name) return;
    const backend = interfaceBackends.get(name) ?? "socketcan";

    const info = await openChannel(backend, name, bitrate);
    if (!info) {
      console.error("Failed to open channel: ", name);
      dialog.close();
      return;
    }
    channelBitrates.set(info.id, bitrate);

    if (dialogPendingDbc) {
      try {
        const dbc = await invoke<ParsedDbc>("load_dbc", { channelId: info.id, path: dialogPendingDbc });
        dbcByChannel.set(info.id, dbc);
      } catch (e) { setStatus(`DBC error: ${e}`); }
    }

    await refreshChannelList();
    setStatus(`Opened channel: ${name}`);
    scheduleAutoSave();
  } else {
    const id = dialogEditTarget!;
    const name = channelDisplayName(id);
    const backend = channelBackend(id);
    const wasOpen = openChannels.includes(id);

    if (wasOpen) {
      try { await invoke("close_channel", { channelId: id }); dbcByChannel.delete(id); } catch {}
    }
    const info = await openChannel(backend, name, bitrate);
    if (!info) { dialog.close(); return; }
    channelBitrates.set(info.id, bitrate);

    if (dialogPendingDbc) {
      try {
        const dbc = await invoke<ParsedDbc>("load_dbc", { channelId: info.id, path: dialogPendingDbc });
        dbcByChannel.set(info.id, dbc);
      } catch (e) { setStatus(`DBC error: ${e}`); }
    }

    await refreshChannelList();
    if (selectedChannel === info.id) renderDbcTree();
    setStatus(`Updated channel: ${name}`);
    scheduleAutoSave();
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

function showFilterMenu(
  x: number, y: number,
  items: { label: string; key: string }[],
  active: Set<string> | null,
  onFilter: (active: Set<string> | null) => void,
) {
  if (ctxMenu) ctxMenu.remove();
  const menu = document.createElement("div");
  menu.className = "ctx-menu filter-menu";
  menu.addEventListener("click", e => e.stopPropagation());

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

function selectChannel(name: string | null) {
  selectedChannel = name;
  renderChannelList();
  renderDbcTree((document.getElementById("signal-search") as HTMLInputElement).value);
}

async function refreshChannelList() {
  try {
    const infos = await invoke<ChannelInfo[]>("get_open_channels");
    openChannels = infos.map(i => i.id);
  } catch { openChannels = []; }
  if (selectedChannel && !openChannels.includes(selectedChannel)) selectChannel(openChannels[0] ?? null);
  else if (!selectedChannel && openChannels.length > 0) selectChannel(openChannels[0]);
  renderChannelList();
  renderSimEntries();
}

function renderChannelList() {
  const list = document.getElementById("channel-list")!;
  list.innerHTML = "";
  for (const id of openChannels) {
    const dbc = dbcByChannel.get(id);
    const bitrate = channelBitrates.get(id);
    const name = channelDisplayName(id);
    const backend = channelBackend(id);
    const isSelected = id === selectedChannel;
    const bitrateLabel = name.startsWith("vcan") ? "vcan" : (bitrate ? `${(bitrate / 1000).toFixed(0)}k` : "—");
    const item = document.createElement("div");
    item.className = `channel-item${isSelected ? " selected" : ""}`;
    item.dataset.channel = id;
    item.innerHTML = `
      <span class="dot"></span>
      <span class="ch-name">${name}<span class="ch-backend label-muted"> ${backend}</span></span>
      <span class="ch-dbc">${dbc ? dbc.path.split("/").pop() : "No DBC"}</span>
      <span class="ch-baud label-muted">${bitrateLabel}</span>
      <button class="btn-close-ch" title="Close channel">×</button>
    `;
    item.addEventListener("click", (e) => {
      if ((e.target as HTMLElement).closest(".btn-close-ch")) return;
      selectChannel(id);
    });
    item.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      showContextMenu(e.clientX, e.clientY, [
        { label: "Configure…", action: () => openChannelDialog("edit", id) },
        { label: "Close Channel", danger: true, action: async () => {
          try {
            await invoke("close_channel", { channelId: id });
            dbcByChannel.delete(id);
            channelBitrates.delete(id);
            if (selectedChannel === id) selectChannel(null);
            await refreshChannelList();
            scheduleAutoSave();
          } catch (err) { setStatus(`Close error: ${err}`); }
        }},
      ]);
    });
    item.querySelector(".btn-close-ch")!.addEventListener("click", async (e) => {
      e.stopPropagation();
      try {
        await invoke("close_channel", { channelId: id });
        dbcByChannel.delete(id);
        channelBitrates.delete(id);
        if (selectedChannel === id) selectChannel(null);
        await refreshChannelList();
        scheduleAutoSave();
      } catch (err) { setStatus(`Close error: ${err}`); }
    });
    list.appendChild(item);
  }
}

// ── Simulate tab ──────────────────────────────────────────────────────────────

interface SimMessageEntry {
  kind: "message";
  channel: string;
  messageId: number;
  messageName: string;
  dlc: number;
  signals: { def: DbcSignal; value: number }[];
  periodMs: number;
  timerId: ReturnType<typeof setInterval> | null;
}

interface SimRawEntry {
  kind: "raw";
  channel: string;
  canId: number;
  isExtended: boolean;
  dlc: number;
  data: number[];
  periodMs: number;
  timerId: ReturnType<typeof setInterval> | null;
}

type SimEntry = SimMessageEntry | SimRawEntry;

const simEntries = new Map<string, SimEntry>();
let rawEntryCounter = 0;

// ── Sim entry element builders ────────────────────────────────────────────────

function createSimEntryEl(key: string, entry: SimEntry): HTMLElement {
  const el = document.createElement("div");
  el.className = "sim-group";
  el.dataset.simKey = key;

  if (entry.kind === "message") {
    const idHex = "0x" + entry.messageId.toString(16).toUpperCase().padStart(3, "0");
    el.innerHTML = `
      <div class="sim-group-header">
        <span class="sim-kind-badge kind-msg">MSG</span>
        <span class="sim-msg-name">${entry.messageName}</span>
        <span class="label-muted sim-msg-id">${idHex}</span>
        <span class="ch-badge">${entry.channel}</span>
        <span class="label-muted">Period</span>
        <input type="number" class="sim-period small-input" value="${entry.periodMs}" min="10">
        <span class="label-muted">ms</span>
        <button class="btn btn-sm sim-toggle">Start</button>
        <button class="btn btn-sm btn-danger sim-remove">✕</button>
      </div>
      <div class="sim-group-body">
        ${entry.signals.map((s, i) => `
          <div class="sim-signal-row">
            <span class="sim-sig-name">${s.def.name}</span>
            <input type="number" class="sim-value-input" data-idx="${i}" value="${s.value}" step="any">
            ${s.def.unit ? `<span class="sim-sig-unit label-muted">${s.def.unit}</span>` : ""}
          </div>`).join("")}
      </div>`;

    el.querySelector<HTMLInputElement>(".sim-period")!.addEventListener("input", (e) => {
      const p = parseInt((e.target as HTMLInputElement).value) || 100;
      if (entry.timerId != null) { stopSim(key); entry.periodMs = p; startSim(key); }
      else entry.periodMs = p;
    });
    el.querySelectorAll<HTMLInputElement>(".sim-value-input").forEach(inp => {
      inp.addEventListener("input", () => {
        entry.signals[parseInt(inp.dataset.idx ?? "0")].value = parseFloat(inp.value) || 0;
      });
    });

  } else {
    const idHex = entry.canId.toString(16).toUpperCase().padStart(3, "0");
    el.innerHTML = `
      <div class="sim-group-header">
        <span class="sim-kind-badge kind-raw">RAW</span>
        <select class="sim-channel-sel">
          ${openChannels.map(ch => `<option value="${ch}"${ch === entry.channel ? " selected" : ""}>${ch}</option>`).join("")}
        </select>
        <span class="label-muted">ID</span>
        <input type="text" class="sim-canid-input small-input" value="${idHex}" maxlength="8" placeholder="hex">
        <label class="sim-ext-label label-muted"><input type="checkbox" class="sim-ext-cb"${entry.isExtended ? " checked" : ""}> Ext</label>
        <span class="label-muted">DLC</span>
        <select class="sim-dlc-sel">
          ${[1,2,3,4,5,6,7,8].map(n => `<option value="${n}"${n === entry.dlc ? " selected" : ""}>${n}</option>`).join("")}
        </select>
        <span class="label-muted">Period</span>
        <input type="number" class="sim-period small-input" value="${entry.periodMs}" min="10">
        <span class="label-muted">ms</span>
        <button class="btn btn-sm sim-toggle">Start</button>
        <button class="btn btn-sm btn-danger sim-remove">✕</button>
      </div>
      <div class="sim-group-body">
        <div class="sim-raw-data-row">
          <span class="label-muted">Data</span>
          <div class="sim-bytes">
            ${entry.data.map((b, i) => `<input type="text" class="sim-byte" data-idx="${i}" value="${b.toString(16).toUpperCase().padStart(2,"0")}" maxlength="2"${i >= entry.dlc ? " disabled" : ""}>`).join("")}
          </div>
        </div>
      </div>`;

    el.querySelector<HTMLSelectElement>(".sim-channel-sel")!.addEventListener("change", (e) => {
      entry.channel = (e.target as HTMLSelectElement).value;
    });
    el.querySelector<HTMLInputElement>(".sim-canid-input")!.addEventListener("input", (e) => {
      entry.canId = parseInt((e.target as HTMLInputElement).value, 16) || 0;
    });
    el.querySelector<HTMLInputElement>(".sim-ext-cb")!.addEventListener("change", (e) => {
      entry.isExtended = (e.target as HTMLInputElement).checked;
    });
    el.querySelector<HTMLSelectElement>(".sim-dlc-sel")!.addEventListener("change", (e) => {
      entry.dlc = parseInt((e.target as HTMLSelectElement).value);
      el.querySelectorAll<HTMLInputElement>(".sim-byte").forEach((inp, i) => {
        inp.disabled = i >= entry.dlc;
      });
    });
    el.querySelector<HTMLInputElement>(".sim-period")!.addEventListener("input", (e) => {
      const p = parseInt((e.target as HTMLInputElement).value) || 100;
      if (entry.timerId != null) { stopSim(key); entry.periodMs = p; startSim(key); }
      else entry.periodMs = p;
    });
    el.querySelectorAll<HTMLInputElement>(".sim-byte").forEach(inp => {
      inp.addEventListener("input", () => {
        entry.data[parseInt(inp.dataset.idx ?? "0")] = parseInt(inp.value, 16) || 0;
      });
      inp.addEventListener("blur", () => {
        const i = parseInt(inp.dataset.idx ?? "0");
        inp.value = entry.data[i].toString(16).toUpperCase().padStart(2, "0");
      });
    });
  }

  el.querySelector(".sim-toggle")!.addEventListener("click", () => {
    entry.timerId != null ? stopSim(key) : startSim(key);
  });
  el.querySelector(".sim-remove")!.addEventListener("click", () => removeSimEntry(key));
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

function addSimSignal(channel: string, sig: DbcSignal) {
  const dbc = dbcByChannel.get(channel);
  if (!dbc) { setStatus("No DBC loaded for this channel"); return; }
  const msg = dbc.messages.find(m => m.signals.some(s => s.name === sig.name));
  if (!msg) return;

  const key = `msg::${channel}::${msg.id}`;
  if (simEntries.has(key)) { setStatus(`Message '${msg.name}' already added`); return; }

  const entry: SimMessageEntry = {
    kind: "message", channel,
    messageId: msg.id, messageName: msg.name, dlc: msg.dlc,
    signals: msg.signals.map(s => ({ def: s, value: s.min ?? 0 })),
    periodMs: 100, timerId: null,
  };
  simEntries.set(key, entry);
  document.getElementById("sim-entries")!.appendChild(createSimEntryEl(key, entry));
  updateSignalHighlights();
  scheduleAutoSave();
}

function addRawFrame() {
  const key = `raw::${++rawEntryCounter}`;
  const entry: SimRawEntry = {
    kind: "raw",
    channel: openChannels[0] ?? "",
    canId: 0x100, isExtended: false, dlc: 8,
    data: new Array(8).fill(0),
    periodMs: 100, timerId: null,
  };
  simEntries.set(key, entry);
  document.getElementById("sim-entries")!.appendChild(createSimEntryEl(key, entry));
}

function removeSimEntry(key: string) {
  const entry = simEntries.get(key);
  if (entry?.timerId != null) clearInterval(entry.timerId);
  simEntries.delete(key);
  document.querySelector(`[data-sim-key="${key}"]`)?.remove();
  updateSignalHighlights();
  scheduleAutoSave();
}

function startSim(key: string) {
  const entry = simEntries.get(key);
  if (!entry || entry.timerId != null) return;
  if (!entry.channel) { setStatus("Select a channel first"); return; }

  if (entry.kind === "message") {
    entry.timerId = setInterval(async () => {
      const signalValues: Record<string, number> = {};
      for (const s of entry.signals) signalValues[s.def.name] = s.value;
      try { await invoke("send_message", { cmd: { channel_id: entry.channel, message_id: entry.messageId, signal_values: signalValues } }); }
      catch (e) { setStatus(`Send error: ${e}`); }
    }, entry.periodMs);
  } else {
    entry.timerId = setInterval(async () => {
      try { await invoke("send_raw_frame", { cmd: { channel_id: entry.channel, can_id: entry.canId, data: entry.data.slice(0, entry.dlc) } }); }
      catch (e) { setStatus(`Send error: ${e}`); }
    }, entry.periodMs);
  }

  const btn = document.querySelector<HTMLButtonElement>(`[data-sim-key="${key}"] .sim-toggle`);
  if (btn) { btn.textContent = "Stop"; btn.classList.add("running"); }
}

function stopSim(key: string) {
  const entry = simEntries.get(key);
  if (!entry || entry.timerId == null) return;
  clearInterval(entry.timerId);
  entry.timerId = null;
  const btn = document.querySelector<HTMLButtonElement>(`[data-sim-key="${key}"] .sim-toggle`);
  if (btn) { btn.textContent = "Start"; btn.classList.remove("running"); }
}

// ── Project ───────────────────────────────────────────────────────────────────

function buildProject(): Project {
  return {
    version: 1,
    channels: openChannels.map(id => ({
      name: channelDisplayName(id),
      backend: channelBackend(id),
      dbc_path: dbcByChannel.get(id)?.path ?? null,
      bitrate: channelBitrates.get(id) ?? null,
    })),
    plot_panes: plotPanes.map(pane => ({
      signals: [...pane.series.values()].map(s => ({ signal_name: s.signalName, channel: s.channel })),
    })),
    simulate_signals: [...simEntries.values()].flatMap(e =>
      e.kind === "message"
        ? e.signals.map(s => ({ signal_name: s.def.name, channel: e.channel, value: s.value, period_ms: e.periodMs }))
        : []
    ),
  };
}

async function saveProject() {
  if (projectPath) {
    try { await invoke("save_project", { path: projectPath, project: buildProject() }); setStatus(`Saved: ${projectPath}`); }
    catch (e) { setStatus(`Save error: ${e}`); }
  } else { await saveProjectAs(); }
}

async function saveProjectAs() {
  try {
    const path = await dialogSave({ filters: [{ name: "CAN Project", extensions: ["canproj"] }] });
    if (!path) return;
    projectPath = path;
    await invoke("save_project", { path, project: buildProject() });
    setStatus(`Saved: ${path}`);
  } catch (e) { setStatus(`Save error: ${e}`); }
}

async function openProject() {
  try {
    const path = await dialogOpen({ filters: [{ name: "CAN Project", extensions: ["canproj"] }], multiple: false });
    if (!path || Array.isArray(path)) return;
    const project = await invoke<Project>("load_project", { path });
    projectPath = path;
    await applyProject(project);
    setStatus(`Loaded: ${path}`);
  } catch (e) { setStatus(`Load error: ${e}`); }
}

async function applyProject(project: Project) {
  for (const ch of project.channels) {
    const backend = ch.backend ?? "socketcan";
    const info = await openChannel(backend, ch.name, ch.bitrate ?? null);
    if (info) channelBitrates.set(info.id, ch.bitrate ?? null);
  }
  await refreshChannelList();

  try {
    const all = await invoke<Record<string, ParsedDbc>>("get_all_dbcs");
    for (const [id, dbc] of Object.entries(all)) dbcByChannel.set(id, dbc);
  } catch { }
  renderChannelList();
  if (selectedChannel) renderDbcTree();

  // Remove existing panes, restore saved ones
  while (plotPanes.length) closePlotPane(plotPanes[0].id);

  for (const paneConfig of project.plot_panes) {
    const pane = createPlotPane();
    for (const entry of paneConfig.signals) {
      const dbc = dbcByChannel.get(entry.channel);
      const sig = dbc?.messages.flatMap(m => m.signals).find(s => s.name === entry.signal_name);
      if (sig) addSignalToPane(pane, entry.channel, sig);
    }
  }

  for (const entry of simEntries.values()) { if (entry.timerId != null) clearInterval(entry.timerId); }
  simEntries.clear();
  document.getElementById("sim-entries")!.innerHTML = "";

  // Group saved signal entries by message so each message becomes one card
  const msgMap = new Map<string, { channel: string; msg: DbcMessage; periodMs: number; values: Map<string, number> }>();
  for (const entry of project.simulate_signals) {
    const dbc = dbcByChannel.get(entry.channel);
    const msg = dbc?.messages.find(m => m.signals.some(s => s.name === entry.signal_name));
    if (!msg) continue;
    const key = `msg::${entry.channel}::${msg.id}`;
    if (!msgMap.has(key)) msgMap.set(key, { channel: entry.channel, msg, periodMs: entry.period_ms, values: new Map() });
    msgMap.get(key)!.values.set(entry.signal_name, entry.value);
  }
  const container = document.getElementById("sim-entries")!;
  for (const [key, { channel, msg, periodMs, values }] of msgMap) {
    const simEntry: SimMessageEntry = {
      kind: "message", channel,
      messageId: msg.id, messageName: msg.name, dlc: msg.dlc,
      signals: msg.signals.map(s => ({ def: s, value: values.get(s.name) ?? s.min ?? 0 })),
      periodMs, timerId: null,
    };
    simEntries.set(key, simEntry);
    container.appendChild(createSimEntryEl(key, simEntry));
  }
}

// ── App recording start / stop ────────────────────────────────────────────────

function startApp() {
  appRunning = true;
  appStartTime = Date.now();
  signalHistory.clear();
  signalLastValues.clear();
  signalMinValues.clear();
  signalMaxValues.clear();

  // Rebuild DBC tree to clear sidebar values
  renderDbcTree((document.getElementById("signal-search") as HTMLInputElement).value);

  // Reset plot pause state
  plotPaused = false;
  const pausePlotBtn = document.getElementById("btn-pause-plot")!;
  pausePlotBtn.textContent = "Pause";
  pausePlotBtn.classList.remove("running");

  // Clear all plot pane data and reset zoom
  for (const pane of plotPanes) {
    for (const s of pane.series.values()) { s.timestamps = []; s.labels = []; s.values = []; s.lastValue = null; }
    (pane.chart.data as { labels: string[] }).labels = [];
    if (pane.zoomed) {
      pane.chart.resetZoom();
      pane.zoomed = false;
      pane.el.querySelector<HTMLButtonElement>(".btn-reset-zoom")!.style.display = "none";
    }
    pane.chart.update("none");
  }

  // Reset trace pause state and clear trace
  tracePaused = false;
  const pauseTraceBtn = document.getElementById("btn-pause-trace")!;
  pauseTraceBtn.textContent = "Pause";
  pauseTraceBtn.classList.remove("running");
  clearTrace();

  const btn = document.getElementById("btn-app-run")!;
  btn.textContent = "■ Stop";
  btn.classList.add("running");
  btn.title = "Stop recording";
  setStatus("Recording started");
}

function stopApp() {
  appRunning = false;
  const btn = document.getElementById("btn-app-run")!;
  btn.textContent = "▶ Start";
  btn.classList.remove("running");
  btn.title = "Start recording";
  setStatus("Recording stopped");
}

async function exportCsv() {
  if (signalHistory.size === 0) { setStatus("No data to export"); return; }
  const path = await dialogSave({
    defaultPath: "can_signals.csv",
    filters: [{ name: "CSV Files", extensions: ["csv"] }],
  });
  if (!path) return;

  const rows: string[] = ["timestamp_ms,elapsed_s,channel,signal_name,value,unit"];
  const allSamples: Array<{ ts: number; channel: string; signalName: string; value: number; unit: string }> = [];

  for (const [key, samples] of signalHistory) {
    const sep = key.indexOf("::");
    const channel = key.substring(0, sep);
    const signalName = key.substring(sep + 2);
    for (const s of samples) allSamples.push({ ts: s.ts, channel, signalName, value: s.value, unit: s.unit });
  }
  allSamples.sort((a, b) => a.ts - b.ts);

  for (const s of allSamples) {
    const elapsed = ((s.ts - appStartTime) / 1000).toFixed(3);
    const unitSafe = s.unit.includes(",") ? `"${s.unit}"` : s.unit;
    rows.push(`${s.ts},${elapsed},${s.channel},${s.signalName},${s.value},${unitSafe}`);
  }

  try {
    await invoke("write_text_file", { path, content: rows.join("\n") });
    setStatus(`Exported ${allSamples.length} samples to CSV`);
  } catch (e) { setStatus(`Export error: ${e}`); }
}

// ── Menu bar ──────────────────────────────────────────────────────────────────

function setupMenuBar() {
  document.querySelectorAll<HTMLElement>(".menu-item").forEach(item => {
    item.querySelector<HTMLButtonElement>(".menu-trigger")!.addEventListener("click", (e) => {
      e.stopPropagation();
      const isOpen = item.classList.contains("open");
      closeAllMenus();
      if (!isOpen) item.classList.add("open");
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
  document.addEventListener("keydown", (e) => {
    if (e.ctrlKey && !e.shiftKey && e.key === "o") { e.preventDefault(); handleMenuAction("open-project"); }
    if (e.ctrlKey && !e.shiftKey && e.key === "s") { e.preventDefault(); handleMenuAction("save-project"); }
    if (e.ctrlKey && e.shiftKey  && e.key === "S") { e.preventDefault(); handleMenuAction("save-as-project"); }
  });

  // Options: Max time controls
  const chkMaxTime    = document.getElementById("chk-max-time") as HTMLInputElement;
  const inputMaxTime  = document.getElementById("input-max-time") as HTMLInputElement;
  const lblMaxTimeUnit = document.getElementById("lbl-max-time-unit") as HTMLElement;
  chkMaxTime.addEventListener("change", () => {
    const show = chkMaxTime.checked;
    inputMaxTime.style.display  = show ? "" : "none";
    lblMaxTimeUnit.style.display = show ? "" : "none";
    maxTimeSecs = show ? (parseInt(inputMaxTime.value) || 60) : null;
  });
  inputMaxTime.addEventListener("change", () => {
    if (chkMaxTime.checked) maxTimeSecs = parseInt(inputMaxTime.value) || 60;
  });
}

function closeAllMenus() {
  document.querySelectorAll(".menu-item.open").forEach(el => el.classList.remove("open"));
}

function handleMenuAction(action: string) {
  switch (action) {
    case "open-project":    openProject(); break;
    case "save-project":    saveProject(); break;
    case "save-as-project": saveProjectAs(); break;
    case "export-csv":      exportCsv(); break;
    case "about": (document.getElementById("dialog-about") as HTMLDialogElement).showModal(); break;
  }
}

// ── Trace tab ─────────────────────────────────────────────────────────────────

interface TraceEntry {
  channel: string;
  canId: number;
  isExtended: boolean;
  dlc: number;
  data: number[];
  messageName: string | null;
  timestampMs: number;
  cycleTimeMs: number | null;
  direction: "rx" | "tx";
}

type TraceMode = "overwrite" | "append";
type TraceDataFormat = "hex" | "dec" | "ascii";
let traceMode: TraceMode = "overwrite";
let traceDataFormat: TraceDataFormat = "hex";
let tracePaused = false;
let traceMaxRows = 1000;

const traceLastTs = new Map<string, number>();
const traceRowEls = new Map<string, HTMLTableRowElement>();
const traceSeenChannels = new Set<string>();
const traceSeenCanIds = new Set<number>();
const traceSeenMsgNames = new Set<string>();
let traceSeenNoMsg = false;
let traceFilterChannels: Set<string> | null = null;
let traceFilterCanIds: Set<number> | null = null;
let traceFilterMsgNames: Set<string> | null = null;
let traceFilterData: (number | null)[] = new Array(8).fill(null);
let traceFilterCycleMin: number | null = null;
let traceFilterCycleMax: number | null = null;
let traceFilterDlcMin: number | null = null;
let traceFilterDlcMax: number | null = null;
let traceFilterDir: Set<string> | null = null;
type TraceSortCol = "ts" | "dir" | "channel" | "canId" | "msg" | "dlc" | "data" | "cycle" | null;
let traceSortCol: TraceSortCol = null;
let traceSortDir: "asc" | "desc" = "asc";
const traceAppendBuffer: TraceEntry[] = [];

function traceKey(channel: string, canId: number, direction: "rx" | "tx") {
  return `${channel}::${canId}::${direction}`;
}

function fmtId(canId: number, isExtended: boolean): string {
  return isExtended
    ? canId.toString(16).toUpperCase().padStart(8, "0") + "x"
    : canId.toString(16).toUpperCase().padStart(3, "0") + "h";
}

function fmtData(data: number[]): string {
  switch (traceDataFormat) {
    case "hex":   return data.map(b => b.toString(16).toUpperCase().padStart(2, "0")).join(" ");
    case "dec":   return data.map(b => b.toString().padStart(3, " ")).join(" ");
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

function fmtPlotLabel(ts: number): string {
  return `${(Math.max(0, ts - appStartTime) / 1000).toFixed(1)}s`;
}

function parseByte(s: string): number | null {
  s = s.trim();
  if (!s) return null;
  const n = (s.startsWith("0x") || /[a-fA-F]/.test(s))
    ? parseInt(s.replace(/^0x/i, ""), 16)
    : parseInt(s, 10);
  return (isNaN(n) || n < 0 || n > 255) ? null : n;
}

function traceRowVisible(channel: string, canId: number, bytes: number[], dir: string, cycleMs: number | null, dlc: number, msgName: string | null = null): boolean {
  if (traceFilterChannels !== null && !traceFilterChannels.has(channel)) return false;
  if (traceFilterCanIds !== null && !traceFilterCanIds.has(canId)) return false;
  if (traceFilterDir !== null && !traceFilterDir.has(dir)) return false;
  if (traceFilterMsgNames !== null && !traceFilterMsgNames.has(msgName ?? "")) return false;
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
    tbody.innerHTML = "";
    for (const entry of traceAppendBuffer) {
      if (traceRowVisible(entry.channel, entry.canId, entry.data, entry.direction, entry.cycleTimeMs, entry.dlc, entry.messageName)) {
        tbody.appendChild(buildTraceRow(entry));
      }
    }
    return;
  }
  // Overwrite mode: toggle visibility on the fixed set of rows.
  for (const tr of Array.from(tbody.rows) as HTMLTableRowElement[]) {
    if ((tr as HTMLTableRowElement).dataset.expand) continue;
    const ch = tr.dataset.channel ?? "";
    const id = parseInt(tr.dataset.canid ?? "0");
    const bytes: number[] = JSON.parse(tr.dataset.bytes ?? "[]");
    const dir = tr.dataset.dir ?? "";
    const cycleMs = tr.dataset.cycle ? parseFloat(tr.dataset.cycle) : null;
    const dlc = parseInt(tr.dataset.dlc ?? "0");
    const msgName = tr.dataset.msg || null;
    const visible = traceRowVisible(ch, id, bytes, dir, cycleMs, dlc, msgName);
    tr.style.display = visible ? "" : "none";
    // If hiding a row that has an open expansion, close it
    if (!visible) {
      const next = tr.nextElementSibling as HTMLTableRowElement | null;
      if (next?.dataset.expand) { next.remove(); tr.classList.remove("trace-row-expanded"); }
    }
  }
}

function applyTraceSort() {
  if (!traceSortCol) return;
  const col = traceSortCol;
  const tbody = document.getElementById("trace-tbody") as HTMLTableSectionElement;
  // Close any open expansion rows — they'd get orphaned during sort
  tbody.querySelectorAll<HTMLTableRowElement>("tr[data-expand]").forEach(r => {
    (r.previousElementSibling as HTMLTableRowElement | null)?.classList.remove("trace-row-expanded");
    r.remove();
  });
  const rows = Array.from(tbody.rows).filter(r => !(r as HTMLTableRowElement).dataset.expand) as HTMLTableRowElement[];
  rows.sort((a, b) => {
    let cmp = 0;
    switch (col) {
      case "ts":      cmp = parseInt(a.dataset.ts ?? "0") - parseInt(b.dataset.ts ?? "0"); break;
      case "dir":     cmp = (a.dataset.dir ?? "").localeCompare(b.dataset.dir ?? ""); break;
      case "channel": cmp = (a.dataset.channel ?? "").localeCompare(b.dataset.channel ?? ""); break;
      case "canId":   cmp = parseInt(a.dataset.canid ?? "0") - parseInt(b.dataset.canid ?? "0"); break;
      case "msg":     cmp = (a.dataset.msg ?? "").localeCompare(b.dataset.msg ?? ""); break;
      case "dlc":     cmp = parseInt(a.dataset.dlc ?? "0") - parseInt(b.dataset.dlc ?? "0"); break;
      case "data": {
        const ba: number[] = JSON.parse(a.dataset.bytes ?? "[]");
        const bb: number[] = JSON.parse(b.dataset.bytes ?? "[]");
        for (let i = 0; i < Math.max(ba.length, bb.length); i++) {
          cmp = (ba[i] ?? -1) - (bb[i] ?? -1);
          if (cmp !== 0) break;
        }
        break;
      }
      case "cycle": {
        const ca = parseFloat(a.dataset.cycle ?? "");
        const cb = parseFloat(b.dataset.cycle ?? "");
        if (isNaN(ca) && isNaN(cb)) cmp = 0;
        else if (isNaN(ca)) cmp = 1;
        else if (isNaN(cb)) cmp = -1;
        else cmp = ca - cb;
        break;
      }
    }
    return traceSortDir === "asc" ? cmp : -cmp;
  });
  for (const row of rows) tbody.appendChild(row);
}

function buildTraceRow(entry: TraceEntry): HTMLTableRowElement {
  const tr = document.createElement("tr");
  tr.dataset.bytes = JSON.stringify(entry.data);
  tr.dataset.channel = entry.channel;
  tr.dataset.canid = String(entry.canId);
  tr.dataset.ts = String(entry.timestampMs);
  tr.dataset.dir = entry.direction;
  tr.dataset.msg = entry.messageName ?? "";
  tr.dataset.dlc = String(entry.dlc);
  tr.dataset.cycle = entry.cycleTimeMs != null ? String(entry.cycleTimeMs) : "";
  if (entry.messageName) tr.classList.add("dbc-match");
  if (!traceRowVisible(entry.channel, entry.canId, entry.data, entry.direction, entry.cycleTimeMs, entry.dlc)) tr.style.display = "none";
  const dirClass = entry.direction === "tx" ? "dir-tx" : "dir-rx";
  tr.innerHTML = `
    <td class="td-ts">${fmtElapsed(entry.timestampMs)}</td>
    <td><span class="dir-badge ${dirClass}">${entry.direction.toUpperCase()}</span></td>
    <td>${channelDisplayName(entry.channel)}</td>
    <td class="td-canid">${fmtId(entry.canId, entry.isExtended)}</td>
    <td>${entry.messageName ?? "<em style='color:var(--text-muted)'>Raw</em>"}</td>
    <td style="text-align:center">${entry.dlc}</td>
    <td class="td-data">${fmtData(entry.data)}</td>
    <td class="td-cycle">${entry.cycleTimeMs != null ? entry.cycleTimeMs.toFixed(1) : "—"}</td>
  `;
  return tr;
}

function updateTraceRowEl(tr: HTMLTableRowElement, entry: TraceEntry) {
  tr.dataset.bytes = JSON.stringify(entry.data);
  tr.dataset.ts = String(entry.timestampMs);
  tr.dataset.dlc = String(entry.dlc);
  tr.dataset.cycle = entry.cycleTimeMs != null ? String(entry.cycleTimeMs) : "";
  tr.style.display = traceRowVisible(entry.channel, entry.canId, entry.data, entry.direction, entry.cycleTimeMs, entry.dlc) ? "" : "none";
  const cells = tr.cells;
  cells[0].textContent = fmtElapsed(entry.timestampMs);
  cells[5].textContent = String(entry.dlc);
  cells[6].textContent = fmtData(entry.data);
  cells[7].textContent = entry.cycleTimeMs != null ? entry.cycleTimeMs.toFixed(1) : "—";

  // Refresh open expansion row with updated signal values + current min/max
  const next = tr.nextElementSibling as HTMLTableRowElement | null;
  if (next?.dataset.expand) {
    const msg = dbcByChannel.get(entry.channel)?.messages.find(m => m.id === entry.canId);
    if (msg) {
      const valCells = next.querySelectorAll<HTMLElement>(".te-val");
      const minCells = next.querySelectorAll<HTMLElement>(".te-min");
      const maxCells = next.querySelectorAll<HTMLElement>(".te-max");
      msg.signals.forEach((sig, i) => {
        if (valCells[i]) valCells[i].textContent = formatSigValue(decodeSignal(entry.data, sig), "");
        const key = plotKey(entry.channel, sig.name);
        const mn = signalMinValues.get(key);
        const mx = signalMaxValues.get(key);
        if (minCells[i]) minCells[i].textContent = mn !== undefined ? formatSigValue(mn, "") : "—";
        if (maxCells[i]) maxCells[i].textContent = mx !== undefined ? formatSigValue(mx, "") : "—";
      });
    }
  }
}

function onCanFrame(ev: CanFrameEvent) {
  if (!appRunning || tracePaused) return;

  traceSeenChannels.add(ev.channel_id);
  traceSeenCanIds.add(ev.can_id);
  const msgNameForFrame = dbcByChannel.get(ev.channel_id)?.messages.find(m => m.id === ev.can_id)?.name ?? null;
  if (msgNameForFrame) traceSeenMsgNames.add(msgNameForFrame);
  else traceSeenNoMsg = true;

  const direction = ev.direction ?? "rx";
  const key = traceKey(ev.channel_id, ev.can_id, direction);
  const prev = traceLastTs.get(key);
  const cycleTime = prev != null ? ev.timestamp_ms - prev : null;
  traceLastTs.set(key, ev.timestamp_ms);

  const dbc = dbcByChannel.get(ev.channel_id);
  const msg = dbc?.messages.find(m => m.id === ev.can_id) ?? null;

  const entry: TraceEntry = {
    channel: ev.channel_id,
    canId: ev.can_id,
    isExtended: ev.is_extended,
    dlc: ev.dlc,
    data: ev.data,
    messageName: msg?.name ?? null,
    timestampMs: ev.timestamp_ms,
    cycleTimeMs: cycleTime,
    direction,
  };

  const tbody = document.getElementById("trace-tbody") as HTMLTableSectionElement;

  if (traceMode === "overwrite") {
    const existing = traceRowEls.get(key);
    if (existing) {
      updateTraceRowEl(existing, entry);
    } else {
      const tr = buildTraceRow(entry);
      traceRowEls.set(key, tr);
      tbody.appendChild(tr);
    }
  } else {
    // Buffer stores the last traceMaxRows entries regardless of filter.
    traceAppendBuffer.unshift(entry);
    if (traceAppendBuffer.length > traceMaxRows) traceAppendBuffer.pop();

    // Only add a DOM row if the entry passes the current filter.
    // The DOM therefore contains only visible rows — no hidden rows, no slow scans.
    if (traceRowVisible(entry.channel, entry.canId, entry.data, entry.direction, entry.cycleTimeMs, entry.dlc, entry.messageName)) {
      const tr = buildTraceRow(entry);
      tbody.insertBefore(tr, tbody.firstChild);
      // DOM visible-row count mirrors the buffer's visible subset; trim from the bottom.
      while (tbody.rows.length > traceMaxRows) tbody.deleteRow(-1);
    }
  }
}

function clearTrace() {
  (document.getElementById("trace-tbody") as HTMLTableSectionElement).innerHTML = "";
  traceRowEls.clear();
  traceLastTs.clear();
  traceSeenChannels.clear();
  traceSeenCanIds.clear();
  traceSeenMsgNames.clear();
  traceSeenNoMsg = false;
  traceAppendBuffer.length = 0;
}

function refreshTraceFormat() {
  const tbody = document.getElementById("trace-tbody") as HTMLTableSectionElement;
  for (const tr of Array.from(tbody.rows)) {
    const bytes: number[] = JSON.parse((tr as HTMLTableRowElement).dataset.bytes ?? "[]");
    tr.cells[6].textContent = fmtData(bytes);
  }
}

function setupTrace() {
  document.getElementById("btn-clear-trace")!.addEventListener("click", clearTrace);

  // ── Trace row expansion ───────────────────────────────────────────────────
  document.getElementById("trace-tbody")!.addEventListener("click", (e) => {
    const tr = (e.target as HTMLElement).closest("tr") as HTMLTableRowElement | null;
    if (!tr || tr.dataset.expand || !tr.classList.contains("dbc-match")) return;

    const next = tr.nextElementSibling as HTMLTableRowElement | null;
    if (next?.dataset.expand) {
      next.remove();
      tr.classList.remove("trace-row-expanded");
      return;
    }

    const channel = tr.dataset.channel ?? "";
    const canId = parseInt(tr.dataset.canid ?? "0");
    const bytes: number[] = JSON.parse(tr.dataset.bytes ?? "[]");
    const msg = dbcByChannel.get(channel)?.messages.find(m => m.id === canId);
    if (!msg) return;

    const expandTr = document.createElement("tr");
    expandTr.dataset.expand = "1";
    const td = document.createElement("td");
    td.colSpan = 8;
    td.className = "trace-expand-cell";

    let html = '<table class="trace-expand-table"><thead><tr>'
      + '<th>Signal</th><th>Value</th><th>Min</th><th>Max</th><th>Unit</th>'
      + '</tr></thead><tbody>';
    for (const sig of msg.signals) {
      const val = decodeSignal(bytes, sig);
      const key = plotKey(channel, sig.name);
      const mn = signalMinValues.get(key);
      const mx = signalMaxValues.get(key);
      const fmt = (v: number | undefined) => v !== undefined ? formatSigValue(v, "") : "—";
      html += `<tr>
        <td class="te-name">${sig.name}</td>
        <td class="te-val">${formatSigValue(val, "")}</td>
        <td class="te-min">${fmt(mn)}</td>
        <td class="te-max">${fmt(mx)}</td>
        <td class="te-unit">${sig.unit || "—"}</td></tr>`;
    }
    html += '</tbody></table>';
    td.innerHTML = html;
    expandTr.appendChild(td);
    tr.after(expandTr);
    tr.classList.add("trace-row-expanded");
  });

  // ── Column header sort + filter ───────────────────────────────────────────
  const headerRow = document.querySelector("#trace-table thead tr")!;
  const ths = Array.from(headerRow.children) as HTMLTableCellElement[];
  const thCols: TraceSortCol[] = ["ts", "dir", "channel", "canId", "msg", "dlc", "data", "cycle"];
  const thLabels = ["Timestamp", "Dir", "Channel", "CAN ID", "Message", "DLC", "Data", "Cycle (ms)"];

  function updateSortIndicators() {
    thCols.forEach((col, i) => {
      const active = traceSortCol === col;
      ths[i].childNodes[0].textContent = thLabels[i] + (active ? (traceSortDir === "asc" ? " ▲" : " ▼") : "");
    });
  }

  function setSortCol(col: TraceSortCol) {
    if (traceSortCol === col) traceSortDir = traceSortDir === "asc" ? "desc" : "asc";
    else { traceSortCol = col; traceSortDir = "asc"; }
    updateSortIndicators();
    applyTraceSort();
  }

  thCols.forEach((col, i) => ths[i].addEventListener("click", () => setSortCol(col)));

  // Message filter (right-click col 4)
  ths[4].addEventListener("contextmenu", (e) => {
    e.preventDefault();
    const items = [...traceSeenMsgNames].sort().map(n => ({ label: n, key: n }));
    if (traceSeenNoMsg) items.push({ label: "(no message)", key: "" });
    if (!items.length) return;
    showFilterMenu(e.clientX, e.clientY, items, traceFilterMsgNames, (active) => {
      traceFilterMsgNames = active;
      ths[4].classList.toggle("th-filtered", active !== null);
      applyTraceFilter();
    });
  });

  // Channel filter (right-click col 2)
  ths[2].addEventListener("contextmenu", (e) => {
    e.preventDefault();
    const items = [...traceSeenChannels].sort().map(ch => ({ label: channelDisplayName(ch), key: ch }));
    if (!items.length) return;
    showFilterMenu(e.clientX, e.clientY, items, traceFilterChannels, (active) => {
      traceFilterChannels = active;
      ths[2].classList.toggle("th-filtered", active !== null);
      applyTraceFilter();
    });
  });

  // CAN ID filter (right-click col 3)
  ths[3].addEventListener("contextmenu", (e) => {
    e.preventDefault();
    const items = [...traceSeenCanIds].sort((a, b) => a - b).map(id => ({
      label: fmtId(id, id > 0x7FF), key: String(id),
    }));
    if (!items.length) return;
    showFilterMenu(e.clientX, e.clientY, items,
      traceFilterCanIds !== null ? new Set([...traceFilterCanIds].map(String)) : null,
      (active) => {
        traceFilterCanIds = active !== null ? new Set([...active].map(Number)) : null;
        ths[3].classList.toggle("th-filtered", active !== null);
        applyTraceFilter();
      });
  });

  // Data byte filter (right-click col 6)
  ths[6].addEventListener("contextmenu", (e) => {
    e.preventDefault();
    if (ctxMenu) ctxMenu.remove();
    const menu = document.createElement("div");
    menu.className = "ctx-menu data-filter-menu";
    menu.addEventListener("click", ev => ev.stopPropagation());

    const inputs: HTMLInputElement[] = [];
    const grid = document.createElement("div");
    grid.className = "data-filter-grid";
    for (let i = 0; i < 8; i++) {
      const cell = document.createElement("div");
      cell.className = "data-filter-cell";
      const lbl = document.createElement("span");
      lbl.className = "data-filter-lbl";
      lbl.textContent = String(i);
      const inp = document.createElement("input");
      inp.type = "text";
      inp.className = "data-filter-inp";
      inp.placeholder = "—";
      inp.maxLength = 4;
      const cur = traceFilterData[i];
      if (cur !== null) inp.value = cur.toString(16).toUpperCase().padStart(2, "0");
      inp.addEventListener("input", () => {
        const val = parseByte(inp.value);
        traceFilterData[i] = val;
        inp.classList.toggle("data-filter-invalid", inp.value.trim() !== "" && val === null);
        const hasAny = traceFilterData.some(v => v !== null);
        ths[6].classList.toggle("th-filtered", hasAny);
        applyTraceFilter();
      });
      cell.append(lbl, inp);
      grid.appendChild(cell);
      inputs.push(inp);
    }
    menu.appendChild(grid);

    const hint = document.createElement("div");
    hint.className = "data-filter-hint";
    hint.textContent = "hex (FF) or decimal (255), empty = any";
    menu.appendChild(hint);

    const clearBtn = document.createElement("button");
    clearBtn.textContent = "Clear all";
    clearBtn.className = "filter-ctrl-btn";
    clearBtn.style.marginTop = "6px";
    clearBtn.style.width = "100%";
    clearBtn.addEventListener("click", () => {
      traceFilterData.fill(null);
      inputs.forEach(inp => { inp.value = ""; inp.classList.remove("data-filter-invalid"); });
      ths[6].classList.remove("th-filtered");
      applyTraceFilter();
    });
    menu.appendChild(clearBtn);

    menu.style.left = `${e.clientX}px`;
    menu.style.top = `${e.clientY}px`;
    document.body.appendChild(menu);
    ctxMenu = menu;
    const rect = menu.getBoundingClientRect();
    if (rect.right > window.innerWidth) menu.style.left = `${e.clientX - rect.width}px`;
    if (rect.bottom > window.innerHeight) menu.style.top = `${e.clientY - rect.height}px`;
  });

  // Dir filter (right-click col 1)
  ths[1].addEventListener("contextmenu", (e) => {
    e.preventDefault();
    showFilterMenu(e.clientX, e.clientY,
      [{ label: "RX", key: "rx" }, { label: "TX", key: "tx" }],
      traceFilterDir,
      (active) => {
        traceFilterDir = active;
        ths[1].classList.toggle("th-filtered", active !== null);
        applyTraceFilter();
      });
  });

  // DLC filter (right-click col 5)
  ths[5].addEventListener("contextmenu", (e) => {
    e.preventDefault();
    showRangeFilterMenu(e.clientX, e.clientY, "DLC", traceFilterDlcMin, traceFilterDlcMax, (mn, mx) => {
      traceFilterDlcMin = mn; traceFilterDlcMax = mx;
      ths[5].classList.toggle("th-filtered", mn !== null || mx !== null);
      applyTraceFilter();
    });
  });

  // Cycle filter (right-click col 7)
  ths[7].addEventListener("contextmenu", (e) => {
    e.preventDefault();
    showRangeFilterMenu(e.clientX, e.clientY, "Cycle (ms)", traceFilterCycleMin, traceFilterCycleMax, (mn, mx) => {
      traceFilterCycleMin = mn; traceFilterCycleMax = mx;
      ths[7].classList.toggle("th-filtered", mn !== null || mx !== null);
      applyTraceFilter();
    });
  });

  document.getElementById("select-trace-format")!.addEventListener("change", (e) => {
    traceDataFormat = (e.target as HTMLSelectElement).value as TraceDataFormat;
    refreshTraceFormat();
  });

  document.querySelectorAll<HTMLInputElement>("input[name='trace-mode']").forEach(radio => {
    radio.addEventListener("change", () => {
      traceMode = radio.value as TraceMode;
      clearTrace();
    });
  });

  document.getElementById("input-trace-max")!.addEventListener("change", (e) => {
    traceMaxRows = parseInt((e.target as HTMLInputElement).value) || 1000;
    while (traceAppendBuffer.length > traceMaxRows) traceAppendBuffer.pop();
  });

  const pauseBtn = document.getElementById("btn-pause-trace")!;
  pauseBtn.addEventListener("click", () => {
    tracePaused = !tracePaused;
    pauseBtn.textContent = tracePaused ? "Resume" : "Pause";
    pauseBtn.classList.toggle("running", tracePaused);
  });
}

// ── Utilities ─────────────────────────────────────────────────────────────────

function setStatus(msg: string) {
  const el = document.getElementById("status-bar")!;
  el.textContent = msg;
  console.log("Status: ", msg);
  setTimeout(() => { if (el.textContent === msg) el.textContent = ""; }, 4000);
}

// ── Boot ──────────────────────────────────────────────────────────────────────

window.addEventListener("DOMContentLoaded", async () => {
  // Tab switching
  document.querySelectorAll<HTMLButtonElement>(".tab-btn").forEach(btn => {
    btn.addEventListener("click", () => {
      document.querySelectorAll(".tab-btn").forEach(b => b.classList.remove("active"));
      document.querySelectorAll(".tab-content").forEach(c => c.classList.remove("active"));
      btn.classList.add("active");
      document.getElementById(`tab-${btn.dataset.tab}`)!.classList.add("active");
    });
  });

  // DBC filter
  document.getElementById("signal-search")!.addEventListener("input", (e) => {
    renderDbcTree((e.target as HTMLInputElement).value);
  });

  // Channel dialog
  const chanDialog = document.getElementById("dialog-channel") as HTMLDialogElement;
  document.getElementById("btn-add-channel")!.addEventListener("click", () => openChannelDialog("add"));
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

  // Plot controls
  document.getElementById("btn-clear-plot")!.addEventListener("click", () => {
    for (const pane of plotPanes) {
      for (const s of pane.series.values()) { s.timestamps = []; s.labels = []; s.values = []; s.lastValue = null; }
      (pane.chart.data as { labels: string[] }).labels = [];
      pane.chart.update("none");
    }
  });

  document.getElementById("btn-add-raw-frame")!.addEventListener("click", addRawFrame);

  // Play / Stop
  document.getElementById("btn-app-run")!.addEventListener("click", () => {
    if (appRunning) stopApp(); else startApp();
  });

  const pauseBtn = document.getElementById("btn-pause-plot")!;
  pauseBtn.addEventListener("click", () => {
    plotPaused = !plotPaused;
    pauseBtn.textContent = plotPaused ? "Resume" : "Pause";
    pauseBtn.classList.toggle("running", plotPaused);
  });

  // Menu bar
  setupMenuBar();

  // Drop zones
  setupDropZone();
  setupSimDrop();

  // Trace
  setupTrace();

  // Events
  await listen<SignalValueEvent>("signal-value", (event) => onSignalValue(event.payload));
  await listen<CanFrameEvent>("can-frame", (event) => onCanFrame(event.payload));

  // Sudo password request from the Rust backend — show dialog once, cache in Rust.
  await listen("request-sudo-password", async () => {
    const pw = await promptSudoPassword();
    await invoke("provide_sudo_password", { password: pw ?? null }).catch(() => {});
  });

  // Resolve session file path, then try to restore the last session
  try {
    const dir = await invoke<string>("get_app_data_dir");
    sessionFilePath = `${dir}/last-session.canproj`;
    const project = await invoke<Project>("load_project", { path: sessionFilePath });
    await applyProject(project);
    setStatus("Session restored");
  } catch {
    // No previous session — start fresh with one empty pane
    createPlotPane();
    await refreshChannelList();
    renderDbcTree();
  }
});
