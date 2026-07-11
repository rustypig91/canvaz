# Canvaz

A desktop application for viewing and simulating CAN bus traffic using DBC files.

## Features

- **Trace** — live frame table with filtering by channel, CAN ID, message name, DLC, direction, cycle time, and data bytes
- **Plot** — real-time signal plots with zoom, multi-signal panes, and drag-and-drop from the DBC browser
- **Simulate** — send DBC-defined messages or raw frames on a configurable interval
- **DBC browser** — load a DBC file per channel, browse messages and signals, see live decoded values (including value-table enums) and min/max
- **J1939** — per-channel protocol mode adding PGN / priority / SA / DA columns to the trace and passive transport-protocol reassembly (BAM and RTS/CTS)
- **Project files** — save and restore your channel, plot, and simulator configuration (`.canvaz`)
- **Session restore** — last working state is restored automatically on startup
- **Offline channels** — channels whose hardware is absent are kept in the configuration and recover automatically once the hardware shows up (or via *Reload backends*)
- **System resources** — built-in dialog showing the app's CPU and memory usage

## Hardware support

| Backend | Platforms | Runtime requirement |
|---------|-----------|---------------------|
| Kvaser CANlib (`canlib32.dll` / `libcanlib.so`) | Windows, Linux | [Kvaser drivers](https://www.kvaser.com/download/) |
| PEAK PCAN-Basic (`PCANBasic.dll` / `libpcanbasic.so`) | Windows, Linux | [PCAN-Basic API](https://www.peak-system.com/products/software/development-packages/pcan-basic/) |
| SocketCAN, including virtual `vcan` | Linux | — |

All backends are included in a default build (SocketCAN automatically on
Linux). Kvaser and PCAN load their driver library at runtime; a missing driver
just disables that backend rather than preventing startup.

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
- CAN decoding: custom DBC parser + signal codec, J1939 ID decoding and TP reassembly in `src-tauri/src/j1939.rs`
