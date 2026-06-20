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
  channel: string;
  signal_name: string;
  message_name: string;
  value: number;
  unit: string;
  timestamp_ms: number;
}

interface CanFrameEvent {
  channel: string;
  can_id: number;
  is_extended: boolean;
  dlc: number;
  data: number[];
  timestamp_ms: number;
}

interface PlotSignalEntry { signal_name: string; channel: string; }
interface PlotPaneConfig  { signals: PlotSignalEntry[]; }
interface ChannelConfig   { name: string; dbc_path: string | null; bitrate?: number | null; }
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
  const key = plotKey(ev.channel, ev.signal_name);

  // Store in global history
  let hist = signalHistory.get(key);
  if (!hist) { hist = []; signalHistory.set(key, hist); }
  hist.push({ ts: ev.timestamp_ms, value: ev.value, unit: ev.unit });
  if (maxTimeSecs !== null) {
    const cutoff = ev.timestamp_ms - maxTimeSecs * 1000;
    while (hist.length > 0 && hist[0].ts < cutoff) hist.shift();
  }

  // Update sidebar value
  signalLastValues.set(key, ev.value);
  const valEl = signalValueEls.get(key);
  if (valEl) {
    valEl.textContent = formatSigValue(ev.value, ev.unit);
    valEl.classList.remove("sig-value--empty");
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

  const simulated = new Set(simRows.keys());

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
      row.innerHTML = `<span class="sig-name">${sig.name}</span><span class="sig-value${lastVal == null ? " sig-value--empty" : ""}">${valText}</span>`;
      signalValueEls.set(key, row.querySelector<HTMLElement>(".sig-value")!);

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

const dbcByChannel = new Map<string, ParsedDbc>();
const channelBitrates = new Map<string, number | null>();
const signalLastValues = new Map<string, number>();   // key → latest value
const signalValueEls   = new Map<string, HTMLElement>(); // key → .sig-value span
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
    const ifaces = await invoke<string[]>("list_can_interfaces").catch(() => [] as string[]);
    sel.innerHTML = ifaces.map(i => `<option>${i}</option>`).join("");
    // Auto-detect vcan from first item
    if (ifaces[0]?.startsWith("vcan")) setBitrateInDialog(null, true);
  } else {
    const name = channelName!;
    title.textContent = `Channel: ${name}`;
    applyBtn.textContent = "Apply";
    ifaceRow.style.display = "none";
    const dbc = dbcByChannel.get(name);
    dialogPendingDbc = dbc?.path ?? null;
    setDbcLabel(dialogPendingDbc);
    setBitrateInDialog(channelBitrates.get(name) ?? null, name.startsWith("vcan"));
    selectChannel(name);
  }

  dialog.showModal();
}

// ── Sudo password prompt ──────────────────────────────────────────────────────

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

// Try `ip link` without sudo; if the backend returns "needs-sudo:" prompt for
// the password and retry once. Returns false if the user cancelled.
async function configureInterface(name: string, bitrate: number | null): Promise<boolean> {
  try {
    await invoke("configure_channel", { name, bitrate, sudoPassword: null });
    return true;
  } catch (e) {
    const msg = String(e);
    if (msg.startsWith("needs-sudo:")) {
      const password = await promptSudoPassword();
      if (password === null) { setStatus("Configuration cancelled."); return false; }
      try {
        await invoke("configure_channel", { name, bitrate, sudoPassword: password });
        return true;
      } catch (e2) { setStatus(`Configure error: ${e2}`); return false; }
    }
    // Non-permission error (e.g. interface already up) — treat as warning and proceed
    setStatus(`Warning: could not configure ${name}: ${msg}`);
    return true;
  }
}

async function applyChannelDialog() {
  const dialog = document.getElementById("dialog-channel") as HTMLDialogElement;
  const bitrate = getBitrateFromDialog();

  if (dialogMode === "add") {
    const custom = (document.getElementById("input-iface-custom") as HTMLInputElement).value.trim();
    const name = custom || (document.getElementById("select-iface") as HTMLSelectElement).value;
    if (!name) return;

    // Configure network interface (bring up with bitrate); prompts for sudo if needed
    const ok = await configureInterface(name, bitrate);
    if (!ok) { dialog.close(); return; }
    // Open channel
    try {
      await invoke("open_channel", { name });
      channelBitrates.set(name, bitrate);
    } catch (e) { setStatus(`Channel error: ${e}`); dialog.close(); return; }

    // Load DBC if selected
    if (dialogPendingDbc) {
      try {
        const dbc = await invoke<ParsedDbc>("load_dbc", { channel: name, path: dialogPendingDbc });
        dbcByChannel.set(name, dbc);
      } catch (e) { setStatus(`DBC error: ${e}`); }
    }

    await refreshChannelList();
    setStatus(`Opened channel: ${name}`);
    scheduleAutoSave();
  } else {
    const name = dialogEditTarget!;
    const wasOpen = openChannels.includes(name);

    // Close → reconfigure → reopen
    if (wasOpen) {
      try { await invoke("close_channel", { name }); dbcByChannel.delete(name); } catch {}
    }
    const ok = await configureInterface(name, bitrate);
    if (!ok) { dialog.close(); return; }
    if (wasOpen) {
      try { await invoke("open_channel", { name }); } catch (e) { setStatus(`Reopen error: ${e}`); }
    }
    channelBitrates.set(name, bitrate);

    // Reload DBC
    if (dialogPendingDbc) {
      try {
        const dbc = await invoke<ParsedDbc>("load_dbc", { channel: name, path: dialogPendingDbc });
        dbcByChannel.set(name, dbc);
      } catch (e) { setStatus(`DBC error: ${e}`); }
    }

    await refreshChannelList();
    if (selectedChannel === name) renderDbcTree();
    setStatus(`Updated channel: ${name}`);
    scheduleAutoSave();
  }

  dialog.close();
}

// ── Channel management ────────────────────────────────────────────────────────

function selectChannel(name: string | null) {
  selectedChannel = name;
  renderChannelList();
  renderDbcTree((document.getElementById("signal-search") as HTMLInputElement).value);
}

async function refreshChannelList() {
  try { openChannels = await invoke<string[]>("get_open_channels"); }
  catch { openChannels = []; }
  if (selectedChannel && !openChannels.includes(selectedChannel)) selectChannel(openChannels[0] ?? null);
  else if (!selectedChannel && openChannels.length > 0) selectChannel(openChannels[0]);
  renderChannelList();
  renderSimTable();
}

function renderChannelList() {
  const list = document.getElementById("channel-list")!;
  list.innerHTML = "";
  for (const name of openChannels) {
    const dbc = dbcByChannel.get(name);
    const bitrate = channelBitrates.get(name);
    const isSelected = name === selectedChannel;
    const bitrateLabel = name.startsWith("vcan") ? "vcan" : (bitrate ? `${(bitrate/1000).toFixed(0)}k` : "—");
    const item = document.createElement("div");
    item.className = `channel-item${isSelected ? " selected" : ""}`;
    item.dataset.channel = name;
    item.innerHTML = `
      <span class="dot"></span>
      <span class="ch-name">${name}</span>
      <span class="ch-dbc">${dbc ? dbc.path.split("/").pop() : "No DBC"}</span>
      <span class="ch-baud label-muted">${bitrateLabel}</span>
      <button class="btn-close-ch" title="Close channel">×</button>
    `;
    item.addEventListener("click", (e) => {
      if ((e.target as HTMLElement).closest(".btn-close-ch")) return;
      openChannelDialog("edit", name);
    });
    item.querySelector(".btn-close-ch")!.addEventListener("click", async (e) => {
      e.stopPropagation();
      try {
        await invoke("close_channel", { name });
        dbcByChannel.delete(name);
        channelBitrates.delete(name);
        if (selectedChannel === name) selectChannel(null);
        await refreshChannelList();
        scheduleAutoSave();
      } catch (err) { setStatus(`Close error: ${err}`); }
    });
    list.appendChild(item);
  }
}

// ── Simulate tab ──────────────────────────────────────────────────────────────

interface SimRow {
  signalName: string;
  messageName: string;
  messageId: number;
  channel: string;
  value: number;
  periodMs: number;
  timerId: ReturnType<typeof setInterval> | null;
}

const simRows = new Map<string, SimRow>();

function simKey(channel: string, signalName: string) { return `${channel}::${signalName}`; }

function addSimSignal(channel: string, sig: DbcSignal) {
  const key = simKey(channel, sig.name);
  if (simRows.has(key)) return;
  simRows.set(key, { signalName: sig.name, messageName: sig.message_name, messageId: sig.message_id, channel, value: sig.min ?? 0, periodMs: 100, timerId: null });
  renderSimTable();
  updateSignalHighlights();
  scheduleAutoSave();
}

function removeSimSignal(key: string) {
  const row = simRows.get(key);
  if (row?.timerId != null) clearInterval(row.timerId);
  simRows.delete(key);
  renderSimTable();
  updateSignalHighlights();
  scheduleAutoSave();
}

function startSim(key: string) {
  const row = simRows.get(key);
  if (!row || row.timerId != null) return;
  if (!row.channel) { setStatus("Select a channel first"); return; }
  row.timerId = setInterval(async () => {
    try { await invoke("send_signal", { cmd: { channel: row.channel, signal_name: row.signalName, value: row.value } }); }
    catch (e) { setStatus(`Send error: ${e}`); }
  }, row.periodMs);
  renderSimTable();
}

function stopSim(key: string) {
  const row = simRows.get(key);
  if (!row || row.timerId == null) return;
  clearInterval(row.timerId);
  row.timerId = null;
  renderSimTable();
}

function renderSimTable() {
  const tbody = document.getElementById("sim-tbody")!;
  tbody.innerHTML = "";
  for (const [key, row] of simRows) {
    const running = row.timerId != null;
    const tr = document.createElement("tr");
    tr.innerHTML = `
      <td>${row.signalName}</td>
      <td>${row.messageName}</td>
      <td><span class="ch-badge">${row.channel}</span></td>
      <td><input type="number" class="sim-value-input" value="${row.value}" step="any" /></td>
      <td><input type="number" class="sim-period-input" value="${row.periodMs}" min="10" /></td>
      <td class="sim-actions">
        <button class="btn btn-sm ${running ? "running" : ""}" data-action="toggle">${running ? "Stop" : "Start"}</button>
        <button class="btn btn-sm btn-danger" data-action="remove">✕</button>
      </td>
    `;
    tr.querySelector<HTMLInputElement>(".sim-value-input")!.addEventListener("input", (e) => { row.value = parseFloat((e.target as HTMLInputElement).value) || 0; });
    tr.querySelector<HTMLInputElement>(".sim-period-input")!.addEventListener("input", (e) => {
      const p = parseInt((e.target as HTMLInputElement).value) || 100;
      if (row.timerId != null) { stopSim(key); row.periodMs = p; startSim(key); } else row.periodMs = p;
    });
    tr.querySelector("[data-action='toggle']")!.addEventListener("click", () => { running ? stopSim(key) : startSim(key); });
    tr.querySelector("[data-action='remove']")!.addEventListener("click", () => removeSimSignal(key));
    tbody.appendChild(tr);
  }
}

// ── Project ───────────────────────────────────────────────────────────────────

function buildProject(): Project {
  return {
    version: 1,
    channels: openChannels.map(name => ({ name, dbc_path: dbcByChannel.get(name)?.path ?? null, bitrate: channelBitrates.get(name) ?? null })),
    plot_panes: plotPanes.map(pane => ({
      signals: [...pane.series.values()].map(s => ({ signal_name: s.signalName, channel: s.channel })),
    })),
    simulate_signals: [...simRows.values()].map(r => ({ signal_name: r.signalName, channel: r.channel, value: r.value, period_ms: r.periodMs })),
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
    await configureInterface(ch.name, ch.bitrate ?? null);
    try {
      await invoke("open_channel", { name: ch.name });
      channelBitrates.set(ch.name, ch.bitrate ?? null);
    } catch { }
  }
  await refreshChannelList();

  try {
    const all = await invoke<Record<string, ParsedDbc>>("get_all_dbcs");
    for (const [ch, dbc] of Object.entries(all)) dbcByChannel.set(ch, dbc);
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

  simRows.clear();
  for (const entry of project.simulate_signals) {
    const dbc = dbcByChannel.get(entry.channel);
    const sig = dbc?.messages.flatMap(m => m.signals).find(s => s.name === entry.signal_name);
    if (sig) {
      simRows.set(simKey(entry.channel, sig.name), {
        signalName: sig.name, messageName: sig.message_name, messageId: sig.message_id,
        channel: entry.channel, value: entry.value, periodMs: entry.period_ms, timerId: null,
      });
    }
  }
  renderSimTable();
}

// ── App recording start / stop ────────────────────────────────────────────────

function startApp() {
  appRunning = true;
  appStartTime = Date.now();
  signalHistory.clear();
  signalLastValues.clear();

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
}

type TraceMode = "overwrite" | "append";
type TraceDataFormat = "hex" | "dec" | "ascii";
let traceMode: TraceMode = "overwrite";
let traceDataFormat: TraceDataFormat = "hex";
let tracePaused = false;
let traceMaxRows = 1000;

const traceLastTs = new Map<string, number>();
const traceRowEls = new Map<string, HTMLTableRowElement>();

function traceKey(channel: string, canId: number) {
  return `${channel}::${canId}`;
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

function buildTraceRow(entry: TraceEntry): HTMLTableRowElement {
  const tr = document.createElement("tr");
  tr.dataset.bytes = JSON.stringify(entry.data);
  if (entry.messageName) tr.classList.add("dbc-match");
  tr.innerHTML = `
    <td class="td-ts">${fmtElapsed(entry.timestampMs)}</td>
    <td>${entry.channel}</td>
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
  const cells = tr.cells;
  cells[0].textContent = fmtElapsed(entry.timestampMs);
  cells[4].textContent = String(entry.dlc);
  cells[5].textContent = fmtData(entry.data);
  cells[6].textContent = entry.cycleTimeMs != null ? entry.cycleTimeMs.toFixed(1) : "—";
}

function onCanFrame(ev: CanFrameEvent) {
  if (!appRunning || tracePaused) return;

  const key = traceKey(ev.channel, ev.can_id);
  const prev = traceLastTs.get(key);
  const cycleTime = prev != null ? ev.timestamp_ms - prev : null;
  traceLastTs.set(key, ev.timestamp_ms);

  const dbc = dbcByChannel.get(ev.channel);
  const msg = dbc?.messages.find(m => m.id === ev.can_id) ?? null;

  const entry: TraceEntry = {
    channel: ev.channel,
    canId: ev.can_id,
    isExtended: ev.is_extended,
    dlc: ev.dlc,
    data: ev.data,
    messageName: msg?.name ?? null,
    timestampMs: ev.timestamp_ms,
    cycleTimeMs: cycleTime,
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
    const tr = buildTraceRow(entry);
    tbody.insertBefore(tr, tbody.firstChild);
    while (tbody.rows.length > traceMaxRows) tbody.deleteRow(-1);
  }
}

function clearTrace() {
  (document.getElementById("trace-tbody") as HTMLTableSectionElement).innerHTML = "";
  traceRowEls.clear();
  traceLastTs.clear();
}

function refreshTraceFormat() {
  const tbody = document.getElementById("trace-tbody") as HTMLTableSectionElement;
  for (const tr of Array.from(tbody.rows)) {
    const bytes: number[] = JSON.parse((tr as HTMLTableRowElement).dataset.bytes ?? "[]");
    tr.cells[5].textContent = fmtData(bytes);
  }
}

function setupTrace() {
  document.getElementById("btn-clear-trace")!.addEventListener("click", clearTrace);

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
