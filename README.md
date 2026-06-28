# Canvaz

A desktop application for viewing and simulating CAN bus traffic using DBC files.

## Features

- **Trace** — live frame table with filtering by channel, CAN ID, message name, DLC, direction, cycle time, and data bytes
- **Plot** — real-time signal plots with zoom, multi-signal panes, and drag-and-drop from the DBC browser
- **Simulate** — send DBC-defined messages or raw frames on a configurable interval
- **DBC browser** — load a DBC file per channel, browse messages and signals, see live decoded values and min/max
- **Project files** — save and restore your channel, plot, and simulator configuration (`.canvaz`)
- **Session restore** — last working state is restored automatically on startup

## Hardware support

| Platform | Backend | Runtime requirement |
|----------|---------|---------------------|
| Windows  | Kvaser CANlib (`canlib32.dll`) | [Kvaser Drivers for Windows](https://www.kvaser.com/download/) |
| Linux    | SocketCAN (`socketcan` feature) | — |

## Building

**Prerequisites:** [Rust](https://rustup.rs/), [Node.js](https://nodejs.org/), [Tauri prerequisites](https://tauri.app/start/prerequisites/)

```sh
npm install
npm run tauri dev      # development
npm run tauri build    # production
```

## Development

- Frontend: vanilla TypeScript + HTML/CSS, bundled with Vite
- Backend: Rust (Tauri v2)
- CAN decoding: custom DBC parser + signal codec
