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
interface PlotPaneConfig { signals: PlotSignalEntry[]; interpolation?: string; show_points?: boolean; }
interface ChannelInfo { id: string; backend: string; name: string; }
interface ChannelConfig { name: string; backend: string; dbc_path: string | null; bitrate?: number | null; }
interface SimulateEntry { signal_name: string; channel: string; value: number; period_ms: number; }

interface SimRawFrameConfig {
  channel: string; can_id: number; is_extended: boolean;
  dlc: number; data: number[]; period_ms: number;
}

interface TraceFiltersConfig {
  channels?: string[] | null;
  can_ids?: number[] | null;
  msg_names?: string[] | null;
  dir?: string[] | null;
  dlc_min?: number | null;
  dlc_max?: number | null;
  cycle_min?: number | null;
  cycle_max?: number | null;
  data?: (number | null)[];
  data_format?: string;
  overwrite?: boolean;
  max_rows?: number | null;
}

interface TraceColumnsConfig {
  order?: string[];
  hidden?: string[];
  widths?: Record<string, number>;
}

interface Project {
  version: number;
  channels: ChannelConfig[];
  plot_panes: PlotPaneConfig[];
  simulate_signals: SimulateEntry[];
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
  unit: string;
  color: string;
  channel: string;
  timestamps: number[];       // absolute ms, used for pruning
  data: { x: number; y: number }[];  // x = elapsed seconds from appStartTime
  lastValue: number | null;
  frozenLength: number | null; // data.length snapshot taken at pause time; null = not frozen
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
  let yMin = Infinity, yMax = -Infinity;
  for (const s of pane.series.values()) {
    const len = s.frozenLength ?? s.data.length;
    if (len === 0) continue;
    const y = s.data[0].y;
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
    if (!appRunning || viewPaused) { scrollRafId = null; return; }
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

// Middle-mouse pan state
let midPan: { startX: number; startMin: number; startMax: number; chartWidth: number } | null = null;


// Tracks how many plot panes reference each signal (key: plotKey).
// Used to subscribe/unsubscribe on the Rust side exactly once per signal.
const signalSubscribeCount = new Map<string, number>();

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
  if (!pane.zoomed) return;
  const zoomOpts = (pane.chart.options.plugins as any).zoom.zoom;
  const saved = zoomOpts.onZoomComplete;
  delete zoomOpts.onZoomComplete;
  pane.chart.resetZoom();
  zoomOpts.onZoomComplete = saved;
  pane.zoomed = false;
  pane.el.querySelector<HTMLButtonElement>(".btn-reset-zoom")!.style.display = "none";
}

function snapshotPlotPanes() {
  for (const pane of plotPanes)
    for (const s of pane.series.values())
      s.frozenLength = s.data.length;
}

function removeSigFromPane(pane: PlotPane, key: string) {
  if (!pane.series.delete(key)) return;
  decrementSubscription(key);
  if (pane.series.size === 0) { closePlotPane(pane.id); return; }
  syncDatasets(pane);
  updatePaneTitle(pane);
  updateSignalHighlights();
  scheduleAutoSave();
}

function createPlotPane(): PlotPane {
  const id = `pane-${++paneCounter}`;

  // Allocate pane object first so legend callbacks can close over it
  const pane: PlotPane = { id, el: null!, chart: null!, series: new Map(), interpolation: 'none', showPoints: false, hoveredDatasetIndex: null, zoomed: false };

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
    scheduleAutoSave();
  });
  el.querySelector<HTMLSelectElement>(".sel-interp")!.addEventListener("change", (e) => {
    pane.interpolation = (e.currentTarget as HTMLSelectElement).value as PlotPane["interpolation"];
    syncDatasets(pane);
    scheduleAutoSave();
  });
  const resetZoomBtn = el.querySelector<HTMLButtonElement>(".btn-reset-zoom")!;
  resetZoomBtn.addEventListener("click", () => {
    for (const p of plotPanes) clearPaneZoom(p);
  });

  const canvas = el.querySelector<HTMLCanvasElement>("canvas")!;

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
    if (!viewPaused) {
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
            onZoomComplete: () => {
              pane.zoomed = true;
              resetZoomBtn.style.display = "";
              if (!viewPaused) {
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

// Decrement the subscription reference count for a signal key. When it reaches
// zero, tell Rust to stop emitting signal-value events for that signal.
function decrementSubscription(key: string) {
  const count = (signalSubscribeCount.get(key) ?? 0) - 1;
  if (count <= 0) {
    signalSubscribeCount.delete(key);
    const sep = key.indexOf("::");
    invoke("unsubscribe_signals", {
      channelId: key.substring(0, sep),
      signalNames: [key.substring(sep + 2)],
    }).catch(() => {});
  } else {
    signalSubscribeCount.set(key, count);
  }
}

function closePlotPane(id: string) {
  const idx = plotPanes.findIndex(p => p.id === id);
  if (idx === -1) return;
  const [pane] = plotPanes.splice(idx, 1);
  // Unsubscribe any signals not present in other panes.
  for (const key of pane.series.keys()) decrementSubscription(key);
  pane.chart.destroy();
  pane.el.remove();
  updateSignalHighlights();
  scheduleAutoSave();
}

// ── Signal → pane ─────────────────────────────────────────────────────────────

async function addSignalToPane(pane: PlotPane, channel: string, sig: DbcSignal) {
  const key = plotKey(channel, sig.name);
  if (pane.series.has(key)) return;
  const color = PLOT_COLORS[pane.series.size % PLOT_COLORS.length];
  const series: PlotSeries = {
    signalName: sig.name, messageName: sig.message_name, unit: sig.unit,
    color, channel, timestamps: [], data: [], lastValue: null, frozenLength: null,
  };
  pane.series.set(key, series);

  const prevCount = signalSubscribeCount.get(key) ?? 0;
  signalSubscribeCount.set(key, prevCount + 1);

  if (prevCount === 0) {
    // First subscriber: fetch history from Rust ring buffer, then subscribe so
    // live events start flowing. Fetching before subscribing avoids overlap.
    try {
      const sinceMs = Date.now() - windowSizeSec * 1000;
      const history = await invoke<Array<{ timestamp_ms: number; value: number }>>(
        "get_signal_history", { channelId: channel, signalName: sig.name, sinceMs }
      );
      for (const sample of history) {
        series.timestamps.push(sample.timestamp_ms);
        series.data.push({ x: (sample.timestamp_ms - appStartTime) / 1000, y: sample.value });
      }
      if (series.data.length > 0) series.lastValue = series.data[series.data.length - 1].y;
    } catch { /* channel not open or no DBC yet — data will stream in via events */ }
    invoke("subscribe_signals", { channelId: channel, signalNames: [sig.name] }).catch(() => {});
  } else {
    // Already subscribed in another pane — copy existing data to avoid an extra
    // round-trip and potential duplicate events from a concurrent history fetch.
    for (const other of plotPanes) {
      if (other === pane) continue;
      const existing = other.series.get(key);
      if (existing) {
        series.timestamps.push(...existing.timestamps);
        series.data.push(...existing.data);
        series.lastValue = existing.lastValue;
        break;
      }
    }
  }

  syncDatasets(pane);
  updatePaneTitle(pane);
  updateSignalHighlights();
  scheduleAutoSave();
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
    ds.data = viewPaused && s.frozenLength !== null ? s.data.slice(0, s.frozenLength) : s.data;
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

// ── Signal value events ───────────────────────────────────────────────────────

function onSignalValue(ev: SignalValueEvent) {
  if (!appRunning) return;
  const key = plotKey(ev.channel_id, ev.signal_name);

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

  const x = (ev.timestamp_ms - appStartTime) / 1000;
  for (const pane of plotPanes) {
    const series = pane.series.get(key);
    if (!series) continue;
    // Guard against duplicates with pre-loaded history: skip events older than
    // the last stored sample (history is loaded before subscribe is called).
    if (series.timestamps.length > 0 &&
        series.timestamps[series.timestamps.length - 1] >= ev.timestamp_ms) continue;
    series.timestamps.push(ev.timestamp_ms);
    series.data.push({ x, y: ev.value });
    series.lastValue = ev.value;
    markPaneDirty(pane); // no-op while viewPaused; data accumulates in series
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
    tree.innerHTML = `<div style="padding:8px 12px;color:var(--text-muted);font-size:11px">${selectedChannel ? "No DBC loaded for this channel" : "Select a channel"
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
    summary.innerHTML = `${msg.name}<span class="msg-id-badge">0x${msg.id.toString(16).toUpperCase().padStart(3, "0")}</span>`;
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
const signalLastValues = new Map<string, number>();
const signalMinValues = new Map<string, number>();
const signalMaxValues = new Map<string, number>();
const signalValueEls = new Map<string, HTMLElement>();
const signalRangeEls = new Map<string, HTMLElement>();
let selectedChannel: string | null = null;

// ── Auto-save / session restore ───────────────────────────────────────────────

let sessionFilePath: string | null = null;
let autoSaveTimer: ReturnType<typeof setTimeout> | null = null;

let projectDirty = false;

function scheduleAutoSave() {
  if (!sessionFilePath) return;
  projectDirty = true;
  updateWindowTitle();
  if (autoSaveTimer) clearTimeout(autoSaveTimer);
  autoSaveTimer = setTimeout(async () => {
    try { await invoke("save_project", { path: sessionFilePath, project: buildProject() }); }
    catch { /* silent — auto-save failures should not interrupt the user */ }
  }, 1000);
}
let openChannels: string[] = [];
// Channels the user has configured (not necessarily open; hardware opens on Start).
let configuredChannels: ChannelConfig[] = [];
let projectPath: string | null = null;
let lastProjectIndexPath: string | null = null;

function persistLastProjectPath(path: string) {
  if (lastProjectIndexPath)
    invoke("write_text_file", { path: lastProjectIndexPath, content: path }).catch(() => { });
}

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

    // Filter out interfaces already present in configuredChannels
    const configured = new Set(configuredChannels.map(c => `${c.backend}:${c.name}`));
    const available = ifaces.filter(i => !configured.has(`${i.backend}:${i.name}`));

    if (available.length === 0) {
      setStatus("All detected interfaces are already added.");
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
        `<optgroup label="${backend}">${names.map(n => `<option value="${n}">${n}</option>`).join("")}</optgroup>`
      ).join("");

    // Auto-detect vcan from first available item
    if (available[0]?.name.startsWith("vcan")) setBitrateInDialog(null, true);
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
    const input = document.getElementById("input-sudo-pw") as HTMLInputElement;
    const form = document.getElementById("form-sudo")!;
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
    const info = await invoke<ChannelInfo>("open_channel", { backendName: backend, channelName: name, bitrate: bitrate });
    // Re-subscribe any signals currently in plots for this channel (Rust clears
    // subscriptions when a channel is closed, so we must restore them on reopen).
    const names: string[] = [];
    for (const [key, count] of signalSubscribeCount) {
      if (count > 0 && key.startsWith(`${info.id}::`)) {
        names.push(key.substring(info.id.length + 2));
      }
    }
    if (names.length > 0) invoke("subscribe_signals", { channelId: info.id, signalNames: names }).catch(() => {});
    return info;
  } catch (e) {
    const msg = String(e);
    if (msg === "Sudo authentication cancelled") {
      setError("Cancelled — sudo password required.");
    } else {
      setError(`Channel error: ${msg}`);
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
    const channelId = `${backend}:${name}`;

    // Add to configured list without opening hardware (hardware opens on Start).
    configuredChannels.push({ name, backend, dbc_path: dialogPendingDbc, bitrate });
    channelBitrates.set(channelId, bitrate);

    if (dialogPendingDbc) {
      try {
        const dbc = await invoke<ParsedDbc>("load_dbc", { channelId, path: dialogPendingDbc });
        dbcByChannel.set(channelId, dbc);
      } catch (e) { setError(`DBC error: ${e}`); }
    }

    refreshChannelList();
    setStatus(`Added channel: ${name}`);
    scheduleAutoSave();
  } else {
    const id = dialogEditTarget!;
    const name = channelDisplayName(id);

    // Update config entry in-place.
    const idx = configuredChannels.findIndex(c => `${c.backend}:${c.name}` === id);
    if (idx >= 0) configuredChannels[idx] = { ...configuredChannels[idx], dbc_path: dialogPendingDbc, bitrate };
    channelBitrates.set(id, bitrate);

    if (dialogPendingDbc) {
      try {
        const dbc = await invoke<ParsedDbc>("load_dbc", { channelId: id, path: dialogPendingDbc });
        dbcByChannel.set(id, dbc);
      } catch (e) { setError(`DBC error: ${e}`); }
    } else {
      dbcByChannel.delete(id);
    }

    refreshChannelList();
    if (selectedChannel === id) renderDbcTree();
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

function refreshChannelList() {
  const allIds = configuredChannels.map(ch => `${ch.backend}:${ch.name}`);
  if (selectedChannel && !allIds.includes(selectedChannel)) selectChannel(allIds[0] ?? null);
  else if (!selectedChannel && allIds.length > 0) selectChannel(allIds[0]);
  renderChannelList();
  renderSimEntries();
}

function renderChannelList() {
  const list = document.getElementById("channel-list")!;
  list.innerHTML = "";
  for (const ch of configuredChannels) {
    const id = `${ch.backend}:${ch.name}`;
    const isOpen = openChannels.includes(id);
    const dbc = dbcByChannel.get(id);
    const bitrate = channelBitrates.get(id);
    const name = ch.name;
    const backend = ch.backend;
    const isSelected = id === selectedChannel;
    const bitrateLabel = name.startsWith("vcan") ? "vcan" : (bitrate ? `${(bitrate / 1000).toFixed(0)}k` : "—");
    const item = document.createElement("div");
    item.className = `channel-item${isSelected ? " selected" : ""}`;
    item.dataset.channel = id;
    item.innerHTML = `
      <span class="dot${isOpen ? "" : " closed"}"></span>
      <span class="ch-name" title="${name}">${name}<span class="ch-backend label-muted"> ${backend}</span></span>
      <span class="ch-dbc"${dbc ? ` title="${dbc.path}"` : ""}>${dbc ? dbc.path.replace(/.*[/\\]/, "") : "No DBC"}</span>
      <span class="ch-baud label-muted">${bitrateLabel}</span>
      <button class="btn-close-ch" title="Remove channel">×</button>
    `;
    item.addEventListener("click", (e) => {
      if ((e.target as HTMLElement).closest(".btn-close-ch")) return;
      selectChannel(id);
    });
    item.addEventListener("contextmenu", async (e) => {
      e.preventDefault();
      showContextMenu(e.clientX, e.clientY, [
        {
          label: "Configure…", action: async () => {
            if (!await confirmAndStop(`Stop live capture to configure "${name}"?`)) return;
            openChannelDialog("edit", id);
          }
        },
        {
          label: "Remove Channel", danger: true, action: async () => {
            if (!await confirmAndStop(`Stop live capture and remove "${name}"?`)) return;
            configuredChannels = configuredChannels.filter(c => `${c.backend}:${c.name}` !== id);
            dbcByChannel.delete(id);
            channelBitrates.delete(id);
            if (selectedChannel === id) selectChannel(null);
            renderChannelList();
            scheduleAutoSave();
          }
        },
      ]);
    });
    item.querySelector(".btn-close-ch")!.addEventListener("click", async (e) => {
      e.stopPropagation();
      if (!await confirmAndStop(`Stop live capture and remove "${name}"?`)) return;
      configuredChannels = configuredChannels.filter(c => `${c.backend}:${c.name}` !== id);
      dbcByChannel.delete(id);
      channelBitrates.delete(id);
      if (selectedChannel === id) selectChannel(null);
      renderChannelList();
      scheduleAutoSave();
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
          ${configuredChannels.map(ch => `${ch.backend}:${ch.name}`).map(id => `<option value="${id}"${id === entry.channel ? " selected" : ""}>${id}</option>`).join("")}
        </select>
        <span class="label-muted">ID</span>
        <input type="text" class="sim-canid-input small-input" value="${idHex}" maxlength="8" placeholder="hex">
        <label class="sim-ext-label label-muted"><input type="checkbox" class="sim-ext-cb"${entry.isExtended ? " checked" : ""}> Ext</label>
        <span class="label-muted">DLC</span>
        <select class="sim-dlc-sel">
          ${[1, 2, 3, 4, 5, 6, 7, 8].map(n => `<option value="${n}"${n === entry.dlc ? " selected" : ""}>${n}</option>`).join("")}
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
            ${entry.data.map((b, i) => `<input type="text" class="sim-byte" data-idx="${i}" value="${b.toString(16).toUpperCase().padStart(2, "0")}" maxlength="2"${i >= entry.dlc ? " disabled" : ""}>`).join("")}
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
    channel: configuredChannels.length > 0 ? `${configuredChannels[0].backend}:${configuredChannels[0].name}` : "",
    canId: 0x100, isExtended: false, dlc: 8,
    data: new Array(8).fill(0),
    periodMs: 100, timerId: null,
  };
  simEntries.set(key, entry);
  document.getElementById("sim-entries")!.appendChild(createSimEntryEl(key, entry));
  scheduleAutoSave();
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
      catch (e) { setError(`Send error: ${e}`); }
    }, entry.periodMs);
  } else {
    entry.timerId = setInterval(async () => {
      try { await invoke("send_raw_frame", { cmd: { channel_id: entry.channel, can_id: entry.canId, data: entry.data.slice(0, entry.dlc) } }); }
      catch (e) { setError(`Send error: ${e}`); }
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
    channels: configuredChannels.map(ch => ({
      name: ch.name,
      backend: ch.backend,
      dbc_path: dbcByChannel.get(`${ch.backend}:${ch.name}`)?.path ?? null,
      bitrate: channelBitrates.get(`${ch.backend}:${ch.name}`) ?? null,
    })),
    plot_panes: plotPanes.map(pane => ({
      signals: [...pane.series.values()].map(s => ({ signal_name: s.signalName, channel: s.channel })),
      interpolation: pane.interpolation,
      show_points: pane.showPoints,
    })),
    simulate_signals: [...simEntries.values()].flatMap(e =>
      e.kind === "message"
        ? e.signals.map(s => ({ signal_name: s.def.name, channel: e.channel, value: s.value, period_ms: e.periodMs }))
        : []
    ),
    simulate_raw_frames: [...simEntries.values()]
      .filter((e): e is SimRawEntry => e.kind === "raw")
      .map(e => ({ channel: e.channel, can_id: e.canId, is_extended: e.isExtended, dlc: e.dlc, data: e.data, period_ms: e.periodMs })),
    trace_filters: {
      channels: traceFilterChannels ? [...traceFilterChannels] : null,
      can_ids: traceFilterCanIds ? [...traceFilterCanIds] : null,
      msg_names: traceFilterMsgNames ? [...traceFilterMsgNames] : null,
      dir: traceFilterDir ? [...traceFilterDir] : null,
      dlc_min: traceFilterDlcMin,
      dlc_max: traceFilterDlcMax,
      cycle_min: traceFilterCycleMin,
      cycle_max: traceFilterCycleMax,
      data: traceFilterData,
      data_format: traceDataFormat,
      overwrite: traceMode === "overwrite",
      max_rows: traceMaxRows,
    },
    window_size_sec: windowSizeSec,
    trace_columns: {
      order: traceColOrder,
      hidden: [...traceColHidden],
      widths: { ...traceColWidths },
    },
  };
}

async function saveProject() {
  if (projectPath) {
    try {
      await invoke("save_project", { path: projectPath, project: buildProject() });
      projectDirty = false;
      updateWindowTitle();
      setStatus(`Saved: ${projectPath}`);
    } catch (e) { setError(`Save error: ${e}`); }
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
    setStatus(`Saved: ${path}`);
  } catch (e) { setError(`Save error: ${e}`); }
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
    setStatus(`Loaded: ${path}`);
  } catch (e) { setError(`Load error: ${e}`); }
}

async function applyProject(project: Project) {
  // Stop any active capture before applying a new project.
  if (appRunning) await stopApp();

  configuredChannels = [];
  openChannels = [];

  for (const ch of project.channels) {
    const backend = ch.backend ?? "socketcan";
    const channelId = `${backend}:${ch.name}`;
    configuredChannels.push({ name: ch.name, backend, dbc_path: ch.dbc_path, bitrate: ch.bitrate ?? null });
    channelBitrates.set(channelId, ch.bitrate ?? null);
    if (ch.dbc_path) {
      try {
        const dbc = await invoke<ParsedDbc>("load_dbc", { channelId, path: ch.dbc_path });
        dbcByChannel.set(channelId, dbc);
      } catch { }
    }
  }

  refreshChannelList();
  if (selectedChannel) renderDbcTree();

  // Remove existing panes, restore saved ones
  while (plotPanes.length) closePlotPane(plotPanes[0].id);

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
    for (const entry of paneConfig.signals) {
      const dbc = dbcByChannel.get(entry.channel);
      const sig = dbc?.messages.flatMap(m => m.signals).find(s => s.name === entry.signal_name);
      if (sig) addSignalToPane(pane, entry.channel, sig);
    }
  }

  setWindowSize(project.window_size_sec ?? DEFAULT_WINDOW_SEC);

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

  // Restore raw sim frames
  for (const raw of project.simulate_raw_frames ?? []) {
    const key = `raw::${++rawEntryCounter}`;
    const entry: SimRawEntry = {
      kind: "raw", channel: raw.channel,
      canId: raw.can_id, isExtended: raw.is_extended,
      dlc: raw.dlc, data: raw.data,
      periodMs: raw.period_ms, timerId: null,
    };
    simEntries.set(key, entry);
    container.appendChild(createSimEntryEl(key, entry));
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
    if (tc.widths) traceColWidths = Object.fromEntries(Object.entries(tc.widths).filter(([k]) => validKeys.has(k)));
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
  th("dlc")?.classList.toggle("th-filtered", traceFilterDlcMin !== null || traceFilterDlcMax !== null);
  th("cycle")?.classList.toggle("th-filtered", traceFilterCycleMin !== null || traceFilterCycleMax !== null);
  th("data")?.classList.toggle("th-filtered", traceFilterData.some(v => v !== null));
}

function restoreTraceFilters(f: TraceFiltersConfig) {
  traceFilterChannels = f.channels ? new Set(f.channels) : null;
  traceFilterCanIds = f.can_ids ? new Set(f.can_ids) : null;
  traceFilterMsgNames = f.msg_names ? new Set(f.msg_names) : null;
  traceFilterDir = f.dir ? new Set(f.dir) : null;
  traceFilterDlcMin = f.dlc_min ?? null;
  traceFilterDlcMax = f.dlc_max ?? null;
  traceFilterCycleMin = f.cycle_min ?? null;
  traceFilterCycleMax = f.cycle_max ?? null;
  const savedData = f.data ?? [];
  traceFilterData = Array.from({ length: 8 }, (_, i) => savedData[i] ?? null);

  if (f.data_format === "hex" || f.data_format === "dec" || f.data_format === "ascii")
    traceDataFormat = f.data_format;

  traceMode = (f.overwrite ?? true) ? "overwrite" : "append";
  const overwriteBtn = document.getElementById("btn-trace-overwrite")!;
  overwriteBtn.classList.toggle("active", traceMode === "overwrite");

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

  syncFilteredHeaders();
  updateClearFiltersBtn();
}

// ── App recording start / stop ────────────────────────────────────────────────

async function startApp() {
  // Open all configured channels (hardware connects here, not when added).
  const newOpen: string[] = [];
  for (const ch of configuredChannels) {
    const info = await openChannel(ch.backend, ch.name, ch.bitrate ?? null);
    if (info) newOpen.push(info.id);
  }
  openChannels = newOpen;
  renderChannelList();

  // Reload DBC files so the latest version on disk is used for this run
  await Promise.all(openChannels.map(async (id) => {
    const path = dbcByChannel.get(id)?.path;
    if (!path) return;
    try {
      const dbc = await invoke<ParsedDbc>("load_dbc", { channelId: id, path });
      dbcByChannel.set(id, dbc);
    } catch (e) {
      setError(`DBC reload failed for ${channelDisplayName(id)}: ${e}`);
    }
  }));

  // Prune simulated message entries whose message no longer exists in the reloaded DBC
  {
    const simContainer = document.getElementById("sim-entries")!;
    for (const [key, entry] of [...simEntries]) {
      if (entry.kind !== "message") continue;
      const msg = dbcByChannel.get(entry.channel)?.messages.find(m => m.id === entry.messageId);
      if (!msg) {
        if (entry.timerId != null) clearInterval(entry.timerId);
        simEntries.delete(key);
        simContainer.querySelector(`[data-sim-key="${key}"]`)?.remove();
      }
    }
  }

  // Prune message names from filter that no longer exist in any DBC
  if (traceFilterMsgNames !== null) {
    const validNames = new Set([...dbcByChannel.values()].flatMap(d => d.messages.map(m => m.name)));
    for (const name of [...traceFilterMsgNames]) {
      if (name !== "" && !validNames.has(name)) traceFilterMsgNames.delete(name);
    }
    if (traceFilterMsgNames.size === 0) traceFilterMsgNames = null;
    traceHeaderEls[4]?.classList.toggle("th-filtered", traceFilterMsgNames !== null);
  }

  appRunning = true;
  appStartTime = Date.now();
  signalLastValues.clear();
  signalMinValues.clear();
  signalMaxValues.clear();

  // Rebuild DBC tree to clear sidebar values
  renderDbcTree((document.getElementById("signal-search") as HTMLInputElement).value);

  // Reset global pause state
  viewPaused = false;
  updatePauseViewBtn();

  // Clear all plot pane data, reset zoom and X-axis bounds
  for (const pane of plotPanes) {
    for (const s of pane.series.values()) { s.timestamps.length = 0; s.data.length = 0; s.lastValue = null; s.frozenLength = null; }
    clearPaneZoom(pane);
    const xScale = (pane.chart.options.scales as any)["x"];
    xScale.min = 0;
    xScale.max = windowSizeSec;
    pane.chart.update();
  }

  clearTrace();
  startScrollLoop();

  const btn = document.getElementById("btn-app-run")!;
  btn.textContent = "■ Stop";
  btn.classList.add("running");
  btn.title = "Pause live capture";
  setStatus("Live capture started");
}

async function stopApp() {
  appRunning = false;
  // Close hardware connections; they will reopen on the next Start.
  for (const id of openChannels) {
    try { await invoke("close_channel", { channelId: id }); } catch { }
  }
  openChannels = [];
  renderChannelList();
  const btn = document.getElementById("btn-app-run")!;
  btn.textContent = "▶ Start";
  btn.classList.remove("running");
  btn.title = "Start live capture";
  setStatus("Stopped");
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

/** Exports the signal history to CSV. Returns true if a file was saved. */
async function exportCsv(): Promise<boolean> {
  const hasData = plotPanes.some(p => [...p.series.values()].some(s => s.data.length > 0));
  if (!hasData) { setStatus("No data to export"); return false; }
  const path = await dialogSave({
    defaultPath: "canvaz.csv",
    filters: [{ name: "CSV Files", extensions: ["csv"] }],
  });
  if (!path) return false;

  const rows: string[] = ["timestamp_ms,elapsed_s,channel,signal_name,value,unit"];
  const allSamples: Array<{ ts: number; channel: string; signalName: string; value: number; unit: string }> = [];

  // Each signal may appear in multiple panes; export it only once.
  const seen = new Set<string>();
  for (const pane of plotPanes) {
    for (const [key, series] of pane.series) {
      if (seen.has(key)) continue;
      seen.add(key);
      for (let i = 0; i < series.timestamps.length; i++) {
        allSamples.push({ ts: series.timestamps[i], channel: series.channel,
          signalName: series.signalName, value: series.data[i].y, unit: series.unit });
      }
    }
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
    return true;
  } catch (e) { setError(`Export error: ${e}`); return false; }
}

// ── Preferences ─────────────────────────────────────────────────────────────
// Per-user global preferences, persisted across restarts in the user's app data
// dir (next to the session file, not in the project file). The whole object is
// loaded into memory on startup and written back as a whole so new keys added
// in the future are preserved. Sidebar width is the first such preference.

interface Preferences {
  sidebarWidth?: number;
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
  }).catch(() => { });
}

// ── Window size (data retention) ─────────────────────────────────────────────
// Signal values older than the window are discarded, bounding memory usage.
// The window size is a per-project setting (stored in the project file).

const WINDOW_PRESETS = new Set([30, 60, 300, 900, 1800]);
const DEFAULT_WINDOW_SEC = 60;
let windowSizeSec = DEFAULT_WINDOW_SEC;

function pruneOldData() {
  if (!appRunning || viewPaused) return; // leave a stopped / frozen chart untouched
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
  invoke("set_window_ms", { ms: windowSizeSec * 1000 }).catch(() => {});
}

// User changed the control: apply and mark the project dirty.
function applyWindowSize(sec: number) {
  setWindowSize(sec);
  scheduleAutoSave();
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
    case "open-project": openProject(); break;
    case "save-project": saveProject(); break;
    case "save-as-project": saveProjectAs(); break;
    case "export-csv": exportCsv(); break;
    case "about":
      invoke<string>("get_version").then(v => { document.getElementById("about-version")!.textContent = v; }).catch(() => { });
      (document.getElementById("dialog-about") as HTMLDialogElement).showModal();
      break;
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
let traceMaxRows = 1000;
let traceHeaderEls: HTMLTableCellElement[] = [];

interface TraceColDef { key: string; label: string; defaultWidth: number; }
const TRACE_COL_DEFS: TraceColDef[] = [
  { key: "ts", label: "Timestamp", defaultWidth: 100 },
  { key: "dir", label: "Dir", defaultWidth: 56 },
  { key: "channel", label: "Channel", defaultWidth: 80 },
  { key: "canId", label: "CAN ID", defaultWidth: 90 },
  { key: "msg", label: "Message", defaultWidth: 160 },
  { key: "dlc", label: "DLC", defaultWidth: 56 },
  { key: "data", label: "Data", defaultWidth: 0 },
  { key: "cycle", label: "Cycle (ms)", defaultWidth: 90 },
];
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
  updateClearFiltersBtn();
  scheduleAutoSave();
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
      case "ts": cmp = parseInt(a.dataset.ts ?? "0") - parseInt(b.dataset.ts ?? "0"); break;
      case "dir": cmp = (a.dataset.dir ?? "").localeCompare(b.dataset.dir ?? ""); break;
      case "channel": cmp = (a.dataset.channel ?? "").localeCompare(b.dataset.channel ?? ""); break;
      case "canId": cmp = parseInt(a.dataset.canid ?? "0") - parseInt(b.dataset.canid ?? "0"); break;
      case "msg": cmp = (a.dataset.msg ?? "").localeCompare(b.dataset.msg ?? ""); break;
      case "dlc": cmp = parseInt(a.dataset.dlc ?? "0") - parseInt(b.dataset.dlc ?? "0"); break;
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

function buildTraceCellHtml(key: string, entry: TraceEntry): string {
  const dirClass = entry.direction === "tx" ? "dir-tx" : "dir-rx";
  switch (key) {
    case "ts": return `<td data-col="ts" class="td-ts">${fmtElapsed(entry.timestampMs)}</td>`;
    case "dir": return `<td data-col="dir"><span class="dir-badge ${dirClass}">${entry.direction.toUpperCase()}</span></td>`;
    case "channel": return `<td data-col="channel">${channelDisplayName(entry.channel)}</td>`;
    case "canId": return `<td data-col="canId" class="td-canid">${fmtId(entry.canId, entry.isExtended)}</td>`;
    case "msg": return `<td data-col="msg"${entry.messageName ? ` title="${entry.messageName}"` : ""}>${entry.messageName ?? "<em style='color:var(--text-muted)'>-</em>"}</td>`;
    case "dlc": return `<td data-col="dlc" style="text-align:center">${entry.dlc}</td>`;
    case "data": return `<td data-col="data" class="td-data">${fmtData(entry.data)}</td>`;
    case "cycle": return `<td data-col="cycle" class="td-cycle">${entry.cycleTimeMs != null ? entry.cycleTimeMs.toFixed(1) : "—"}</td>`;
    default: return `<td data-col="${key}"></td>`;
  }
}

function entryFromRow(tr: HTMLTableRowElement): TraceEntry {
  return {
    data: JSON.parse(tr.dataset.bytes ?? "[]"),
    channel: tr.dataset.channel ?? "",
    canId: parseInt(tr.dataset.canid ?? "0"),
    isExtended: tr.dataset.ext === "1",
    timestampMs: parseInt(tr.dataset.ts ?? "0"),
    direction: (tr.dataset.dir as "rx" | "tx") ?? "rx",
    messageName: tr.dataset.msg || null,
    dlc: parseInt(tr.dataset.dlc ?? "0"),
    cycleTimeMs: tr.dataset.cycle ? parseFloat(tr.dataset.cycle) : null,
  };
}

function buildTraceRow(entry: TraceEntry): HTMLTableRowElement {
  const tr = document.createElement("tr");
  tr.dataset.bytes = JSON.stringify(entry.data);
  tr.dataset.channel = entry.channel;
  tr.dataset.canid = String(entry.canId);
  tr.dataset.ext = entry.isExtended ? "1" : "0";
  tr.dataset.ts = String(entry.timestampMs);
  tr.dataset.dir = entry.direction;
  tr.dataset.msg = entry.messageName ?? "";
  tr.dataset.dlc = String(entry.dlc);
  tr.dataset.cycle = entry.cycleTimeMs != null ? String(entry.cycleTimeMs) : "";
  if (entry.messageName) tr.classList.add("dbc-match");
  if (!traceRowVisible(entry.channel, entry.canId, entry.data, entry.direction, entry.cycleTimeMs, entry.dlc, entry.messageName)) tr.style.display = "none";
  const visible = traceColOrder.filter(k => !traceColHidden.has(k));
  tr.innerHTML = visible.map(k => buildTraceCellHtml(k, entry)).join("");
  return tr;
}

function updateTraceRowEl(tr: HTMLTableRowElement, entry: TraceEntry) {
  tr.dataset.bytes = JSON.stringify(entry.data);
  tr.dataset.ts = String(entry.timestampMs);
  tr.dataset.dlc = String(entry.dlc);
  tr.dataset.cycle = entry.cycleTimeMs != null ? String(entry.cycleTimeMs) : "";
  tr.style.display = traceRowVisible(entry.channel, entry.canId, entry.data, entry.direction, entry.cycleTimeMs, entry.dlc, entry.messageName) ? "" : "none";
  const gc = (k: string) => tr.querySelector<HTMLTableCellElement>(`[data-col="${k}"]`);
  const tsCell = gc("ts"); if (tsCell) tsCell.textContent = fmtElapsed(entry.timestampMs);
  const dlcCell = gc("dlc"); if (dlcCell) dlcCell.textContent = String(entry.dlc);
  const dataCell = gc("data"); if (dataCell) dataCell.textContent = fmtData(entry.data);
  const cycleCell = gc("cycle"); if (cycleCell) cycleCell.textContent = entry.cycleTimeMs != null ? entry.cycleTimeMs.toFixed(1) : "—";

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
  if (!appRunning) return;

  // Always update seen sets (for filter autocomplete) and cycle timing.
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

  if (traceMode === "append") {
    traceAppendBuffer.unshift(entry);
    if (traceAppendBuffer.length > traceMaxRows) traceAppendBuffer.pop();
  }

  if (viewPaused) {
    // Accumulate latest state per key; DOM stays frozen until resume.
    if (traceMode === "overwrite") tracePendingOverwrite.set(key, entry);
    return;
  }

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
    if (traceRowVisible(entry.channel, entry.canId, entry.data, entry.direction, entry.cycleTimeMs, entry.dlc, entry.messageName)) {
      const tr = buildTraceRow(entry);
      tbody.insertBefore(tr, tbody.firstChild);
      while (tbody.rows.length > traceMaxRows) tbody.deleteRow(-1);
    }
  }
}

function clearTrace() {
  (document.getElementById("trace-tbody") as HTMLTableSectionElement).innerHTML = "";
  traceRowEls.clear();
  tracePendingOverwrite.clear();
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
    const dc = (tr as HTMLTableRowElement).querySelector<HTMLTableCellElement>('[data-col="data"]');
    if (dc) dc.textContent = fmtData(bytes);
  }
}

function anyFilterActive(): boolean {
  return traceFilterChannels !== null
    || traceFilterCanIds !== null
    || traceFilterMsgNames !== null
    || traceFilterDir !== null
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

function rebuildTraceColumns() {
  const visible = traceColOrder.filter(k => !traceColHidden.has(k));

  const colgroup = document.querySelector("#trace-table colgroup")!;
  colgroup.innerHTML = visible.map(k => {
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

  ths.forEach((th, i) => {
    const key = th.dataset.col ?? "";

    // Resize handle
    const handle = document.createElement("span");
    handle.className = "col-resizer";
    handle.addEventListener("click", (e) => e.stopPropagation());
    handle.addEventListener("mousedown", (e) => {
      e.preventDefault(); e.stopPropagation();
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
            scheduleAutoSave();
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
        const items = [...traceSeenChannels].sort().map(ch => ({ label: channelDisplayName(ch), key: ch }));
        if (!items.length) return;
        showFilterMenu(e.clientX, e.clientY, items, traceFilterChannels, (active) => {
          traceFilterChannels = active; syncFilteredHeaders(); applyTraceFilter();
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
            btn.classList.add("active");
            refreshTraceFormat(); scheduleAutoSave();
          });
          fmtRow.appendChild(btn);
        }
        menu.appendChild(fmtRow);
        const sep = document.createElement("div"); sep.className = "data-filter-sep"; menu.appendChild(sep);
        const inputs: HTMLInputElement[] = [];
        const grid = document.createElement("div"); grid.className = "data-filter-grid";
        for (let idx = 0; idx < 8; idx++) {
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

function setupTrace() {
  rebuildTraceColumns();

  document.getElementById("btn-clear-trace")!.addEventListener("click", clearTrace);
  document.getElementById("btn-clear-filters")!.addEventListener("click", () => {
    clearAllFilters();
    scheduleAutoSave();
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
    for (const def of TRACE_COL_DEFS) {
      const item = document.createElement("label");
      item.className = "ctx-col-item";
      const cb = document.createElement("input");
      cb.type = "checkbox";
      cb.checked = !traceColHidden.has(def.key);
      cb.addEventListener("change", () => {
        if (cb.checked) traceColHidden.delete(def.key);
        else traceColHidden.add(def.key);
        rebuildTraceColumns();
        scheduleAutoSave();
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
      scheduleAutoSave();
      ctxMenu?.remove(); ctxMenu = null;
    });
    menu.appendChild(resetBtn);
    ctxMenu = menu;
    document.body.appendChild(menu);
  });

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
    td.colSpan = traceColOrder.filter(k => !traceColHidden.has(k)).length;
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

  document.getElementById("btn-trace-overwrite")!.addEventListener("click", function () {
    const active = this.classList.toggle("active");
    traceMode = active ? "overwrite" : "append";
    clearTrace();
    scheduleAutoSave();
  });

  document.getElementById("input-trace-max")!.addEventListener("change", (e) => {
    traceMaxRows = parseInt((e.target as HTMLInputElement).value) || 1000;
    while (traceAppendBuffer.length > traceMaxRows) traceAppendBuffer.pop();
    scheduleAutoSave();
  });

}

// ── Global pause ─────────────────────────────────────────────────────────────

function updatePauseViewBtn() {
  const btn = document.getElementById("btn-pause-view")!;
  btn.textContent = viewPaused ? "Resume" : "Pause";
  btn.classList.toggle("running", viewPaused);
}

function resumeFromPause() {
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
  } else {
    // Re-render visible rows from the accumulated buffer (newest first).
    tbody.innerHTML = "";
    const frag = document.createDocumentFragment();
    let count = 0;
    for (const e of traceAppendBuffer) {
      if (count >= traceMaxRows) break;
      if (traceRowVisible(e.channel, e.canId, e.data, e.direction, e.cycleTimeMs, e.dlc, e.messageName)) {
        frag.appendChild(buildTraceRow(e));
        count++;
      }
    }
    tbody.appendChild(frag); // buffer is newest-first, so newest lands at top
  }

  // Bring all plot panes up to date with accumulated data; clear any zoom.
  const now = (Date.now() - appStartTime) / 1000;
  for (const pane of plotPanes) {
    const seriesArray = [...pane.series.values()];
    for (let i = 0; i < seriesArray.length; i++) {
      seriesArray[i].frozenLength = null;
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

interface LogEntry { ts: string; text: string; isError: boolean; }
const messageLog: LogEntry[] = [];

function updateWindowTitle() {
  const base = projectPath ? `Canvaz — ${projectPath}` : "Canvaz";
  getCurrentWindow().setTitle(projectDirty ? `${base} ●` : base);
}

function setStatus(msg: string, isError = false) {
  const el = document.getElementById("status-bar")!;
  el.textContent = msg;
  el.classList.toggle("status-error", isError);
  console.log(isError ? "Error: " : "Status: ", msg);

  const entry: LogEntry = { ts: new Date().toLocaleTimeString(), text: msg, isError };
  messageLog.push(entry);
  appendLogEntry(entry);

  if (isError) {
    document.getElementById("btn-show-log")?.classList.add("log-has-error");
  }

  setTimeout(() => {
    if (el.textContent === msg) {
      el.textContent = "";
      el.classList.remove("status-error");
    }
  }, 4000);
}

function setError(msg: string) { setStatus(msg, true); }

function appendLogEntry(entry: LogEntry) {
  const container = document.getElementById("log-entries");
  if (!container) return;
  const div = document.createElement("div");
  div.className = `log-entry${entry.isError ? " log-error" : ""}`;
  const ts = document.createElement("span");
  ts.className = "log-ts";
  ts.textContent = entry.ts;
  const text = document.createElement("span");
  text.textContent = entry.text;
  div.appendChild(ts);
  div.appendChild(text);
  container.appendChild(div);
  container.scrollTop = container.scrollHeight;
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
    viewPaused = !viewPaused;
    updatePauseViewBtn();
    if (viewPaused) snapshotPlotPanes();
    else resumeFromPause();
  });

  // Log panel
  document.getElementById("btn-show-log")!.addEventListener("click", () => {
    const panel = document.getElementById("log-panel")!;
    panel.hidden = !panel.hidden;
    if (!panel.hidden) {
      document.getElementById("btn-show-log")!.classList.remove("log-has-error");
      const entries = document.getElementById("log-entries")!;
      entries.scrollTop = entries.scrollHeight;
    }
  });
  document.getElementById("btn-close-log")!.addEventListener("click", () => {
    document.getElementById("log-panel")!.hidden = true;
  });
  document.getElementById("btn-clear-log")!.addEventListener("click", () => {
    messageLog.length = 0;
    document.getElementById("log-entries")!.innerHTML = "";
  });
  document.addEventListener("pointerdown", (e) => {
    const panel = document.getElementById("log-panel")!;
    if (panel.hidden) return;
    if (!panel.contains(e.target as Node) && e.target !== document.getElementById("btn-show-log")) {
      panel.hidden = true;
    }
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape") document.getElementById("log-panel")!.hidden = true;
  });

  // Menu bar
  setupMenuBar();

  // Preferences (per-user, persisted across restarts)
  await loadPreferences();
  applySidebarWidth();
  setupSidebarResize();
  setupWindowSize();

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
    await invoke("provide_sudo_password", { password: pw ?? null }).catch(() => { });
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
    setStatus("Session restored");
  } else {
    sessionFilePath = sess;
    // No previous session — start fresh with one empty pane
    createPlotPane();
    refreshChannelList();
    renderDbcTree();
  }

  // Auto-start only when at least one channel has been configured.
  if (configuredChannels.length > 0) {
    await startApp();
  }
  // Otherwise the app stays in stopped state; user adds channels then presses Start.
});
