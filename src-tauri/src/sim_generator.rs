//! Time-varying payloads for simulated periodic messages: per-signal value
//! generators (ramp/sine/toggle), auto-incrementing counters, and checksum
//! bytes. The frontend describes generators per signal name; this module turns
//! them into a [`FrameDataSource`] closure the TX loop evaluates right before
//! every send, so waveforms stay jitter-free even when the UI is busy.

use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::can_communication::FrameDataSource;
use crate::dbc_parser::{pack_bits, ParsedMessage};

/// Per-signal value generator, selected in the simulator UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum SignalGen {
    /// Sawtooth: physical value climbs from `min` to `max` over `period_ms`,
    /// then wraps back to `min`.
    Ramp { min: f64, max: f64, period_ms: u64 },
    /// Sine between `min` and `max` with period `period_ms`, starting at the
    /// midpoint going up.
    Sine { min: f64, max: f64, period_ms: u64 },
    /// Square wave: `min` for the first half of each period, `max` for the second.
    Toggle { min: f64, max: f64, period_ms: u64 },
    /// Raw value increments by 1 per sent frame, wrapping at 2^length —
    /// the message counter of E2E-protected frames.
    Counter,
    /// Computed over the whole frame with this signal's own bits zeroed, after
    /// every other signal (including counters) is encoded, then written into
    /// the signal's bit range.
    Checksum { algorithm: ChecksumAlgorithm },
}

impl SignalGen {
    /// Waveform value at `t_ms` since generator start; `None` for the
    /// post-encode kinds (counter/checksum) which don't produce a physical value.
    fn waveform_value(&self, t_ms: f64) -> Option<f64> {
        let (min, max, period_ms) = match *self {
            SignalGen::Ramp { min, max, period_ms }
            | SignalGen::Sine { min, max, period_ms }
            | SignalGen::Toggle { min, max, period_ms } => (min, max, period_ms),
            SignalGen::Counter | SignalGen::Checksum { .. } => return None,
        };
        if period_ms == 0 {
            return Some(min);
        }
        let phase = (t_ms / period_ms as f64).fract();
        Some(match self {
            SignalGen::Ramp { .. } => min + (max - min) * phase,
            SignalGen::Sine { .. } => min + (max - min) * (0.5 + 0.5 * (std::f64::consts::TAU * phase).sin()),
            SignalGen::Toggle { .. } => {
                if phase < 0.5 {
                    min
                } else {
                    max
                }
            }
            _ => unreachable!(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumAlgorithm {
    /// XOR of all frame bytes.
    Xor8,
    /// Sum of all frame bytes, modulo 256.
    Sum8,
    /// CRC-8 SAE J1850: poly 0x1D, init 0xFF, XOR-out 0xFF — the AUTOSAR E2E
    /// profile 1 CRC.
    Crc8Sae,
}

fn checksum(alg: ChecksumAlgorithm, data: &[u8]) -> u8 {
    match alg {
        ChecksumAlgorithm::Xor8 => data.iter().fold(0, |a, b| a ^ b),
        ChecksumAlgorithm::Sum8 => data.iter().fold(0u8, |a, &b| a.wrapping_add(b)),
        ChecksumAlgorithm::Crc8Sae => {
            let mut crc: u8 = 0xFF;
            for &byte in data {
                crc ^= byte;
                for _ in 0..8 {
                    crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x1D } else { crc << 1 };
                }
            }
            crc ^ 0xFF
        }
    }
}

/// Stamp counter and checksum signals into an already-encoded frame. Counters
/// go first so checksums cover the live counter value; both respect the
/// multiplexer gate the same way `encode_signals` does (writing a signal of an
/// inactive mux group would corrupt the active group's shared bits).
fn apply_generated(
    msg: &ParsedMessage,
    gens: &HashMap<String, SignalGen>,
    values: &HashMap<String, f64>,
    counters: &mut HashMap<String, u64>,
    buf: &mut [u8],
) {
    let mux_raw = msg.multiplexor().map(|m| {
        let v = values.get(&m.name).copied().unwrap_or(0.0);
        if m.factor == 0.0 {
            0
        } else {
            ((v - m.offset) / m.factor).round() as i64
        }
    });
    let active = |mux_value: Option<i64>| match mux_value {
        None => true,
        Some(v) => mux_raw == Some(v),
    };

    for sig in &msg.signals {
        if !active(sig.mux_value) {
            continue;
        }
        if let Some(SignalGen::Counter) = gens.get(&sig.name) {
            let c = counters.entry(sig.name.clone()).or_insert(0);
            // pack_bits writes only the low `length` bits, so the wire value
            // wraps at 2^length by construction.
            pack_bits(buf, *c, sig.start_bit, sig.length, sig.little_endian);
            *c = c.wrapping_add(1);
        }
    }
    for sig in &msg.signals {
        if !active(sig.mux_value) {
            continue;
        }
        if let Some(SignalGen::Checksum { algorithm }) = gens.get(&sig.name) {
            pack_bits(buf, 0, sig.start_bit, sig.length, sig.little_endian);
            let sum = checksum(*algorithm, buf);
            pack_bits(buf, sum as u64, sig.start_bit, sig.length, sig.little_endian);
        }
    }
}

/// Build the per-tick payload closure for a periodic message with generators.
/// `values` holds the constant (UI-entered) physical values; signals present in
/// `gens` override or post-process them. Waveform phase and counter state start
/// fresh — callers rebuild the source on every parameter change.
pub fn build_frame_source(
    msg: ParsedMessage,
    values: HashMap<String, f64>,
    gens: HashMap<String, SignalGen>,
) -> FrameDataSource {
    let start = Instant::now();
    let mut counters: HashMap<String, u64> = HashMap::new();
    Box::new(move || {
        let t_ms = start.elapsed().as_secs_f64() * 1000.0;
        let mut vals = values.clone();
        for (name, gen) in &gens {
            if let Some(v) = gen.waveform_value(t_ms) {
                vals.insert(name.clone(), v);
            }
        }
        let mut buf = msg.encode_signals(&vals);
        apply_generated(&msg, &gens, &vals, &mut counters, &mut buf);
        buf
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbc_parser::ParsedDbc;

    /// 8-byte message: Speed (16 bit @ 0), Count (4 bit @ 16), Crc (8 bit @ 56).
    fn gen_dbc() -> ParsedDbc {
        let path = std::env::temp_dir().join("canvaz_test_gen.dbc");
        let content = "VERSION \"\"\n\nNS_ :\n\nBS_:\n\nBU_:\n\nBO_ 512 GenMsg: 8 Vector__XXX\n \
             SG_ Speed : 0|16@1+ (0.1,0) [0|6553.5] \"kmh\" Vector__XXX\n \
             SG_ Count : 16|4@1+ (1,0) [0|15] \"\" Vector__XXX\n \
             SG_ Crc : 56|8@1+ (1,0) [0|255] \"\" Vector__XXX\n";
        std::fs::write(&path, content).expect("write temp dbc");
        ParsedDbc::new(path.to_str().unwrap()).expect("parse dbc")
    }

    #[test]
    fn crc8_sae_j1850_check_value() {
        // Standard CRC catalogue check value for CRC-8/SAE-J1850.
        assert_eq!(checksum(ChecksumAlgorithm::Crc8Sae, b"123456789"), 0x4B);
        assert_eq!(checksum(ChecksumAlgorithm::Xor8, &[0x12, 0x34, 0x56]), 0x12 ^ 0x34 ^ 0x56);
        assert_eq!(checksum(ChecksumAlgorithm::Sum8, &[0xFF, 0x02]), 0x01, "sum wraps mod 256");
    }

    #[test]
    fn waveforms_hit_expected_points() {
        let ramp = SignalGen::Ramp { min: 0.0, max: 100.0, period_ms: 1000 };
        assert_eq!(ramp.waveform_value(0.0), Some(0.0));
        assert_eq!(ramp.waveform_value(500.0), Some(50.0));
        assert_eq!(ramp.waveform_value(1500.0), Some(50.0), "wraps each period");

        let sine = SignalGen::Sine { min: -10.0, max: 10.0, period_ms: 1000 };
        assert!(sine.waveform_value(0.0).unwrap().abs() < 1e-9, "starts at midpoint");
        assert!((sine.waveform_value(250.0).unwrap() - 10.0).abs() < 1e-9, "peak at quarter period");

        let toggle = SignalGen::Toggle { min: 0.0, max: 1.0, period_ms: 100 };
        assert_eq!(toggle.waveform_value(0.0), Some(0.0));
        assert_eq!(toggle.waveform_value(60.0), Some(1.0));

        // Degenerate period must not divide by zero.
        let flat = SignalGen::Ramp { min: 5.0, max: 9.0, period_ms: 0 };
        assert_eq!(flat.waveform_value(123.0), Some(5.0));
    }

    #[test]
    fn counter_increments_and_wraps_at_signal_width() {
        let dbc = gen_dbc();
        let msg = dbc.messages.get(&512).expect("message");
        let gens: HashMap<String, SignalGen> = [("Count".to_string(), SignalGen::Counter)].into();
        let values: HashMap<String, f64> = [("Speed".to_string(), 100.0)].into();
        let mut source = build_frame_source(msg.clone(), values, gens);

        // 4-bit counter: values 0..=15 then wrap to 0.
        for expected in (0..18).map(|i| i % 16) {
            let buf = source();
            assert_eq!((buf[2] & 0x0F) as u64, expected, "counter in low nibble of byte 2");
            assert_eq!(u16::from_le_bytes([buf[0], buf[1]]), 1000, "Speed 100 kmh = raw 1000 stays encoded");
        }
    }

    #[test]
    fn checksum_covers_frame_with_own_bits_zeroed() {
        let dbc = gen_dbc();
        let msg = dbc.messages.get(&512).expect("message");
        let gens: HashMap<String, SignalGen> = [
            ("Count".to_string(), SignalGen::Counter),
            (
                "Crc".to_string(),
                SignalGen::Checksum {
                    algorithm: ChecksumAlgorithm::Xor8,
                },
            ),
        ]
        .into();
        let values: HashMap<String, f64> = [("Speed".to_string(), 250.0)].into();
        let mut source = build_frame_source(msg.clone(), values, gens);

        let buf = source();
        let expected: u8 = buf[..7].iter().fold(0, |a, b| a ^ b);
        assert_eq!(buf[7], expected, "XOR checksum over the other bytes");

        // Second tick: counter changed, checksum must follow.
        let buf2 = source();
        assert_ne!(buf2[2] & 0x0F, buf[2] & 0x0F, "counter advanced");
        let expected2: u8 = buf2[..7].iter().fold(0, |a, b| a ^ b);
        assert_eq!(buf2[7], expected2);
    }
}
