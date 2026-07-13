use std::collections::HashMap;

use can_dbc::{
    AttributeValue, ByteOrder, Comment, Dbc, MessageId, MultiplexIndicator, NumericValue, SignalExtendedValueType,
    ValueType,
};
use serde::{Deserialize, Serialize};

use crate::can_communication::CanFrame;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedDbc {
    pub path: String,
    /// Keyed by CAN id. Serializes to a JSON object; the frontend treats it as a map.
    pub messages: HashMap<u32, ParsedMessage>,
    /// J1939 (PGN, source address) → key into `messages`, for matching on J1939
    /// channels where the frame's priority bits (and destination address for
    /// PDU1 groups) vary while the DBC lists one fixed 29-bit id per message.
    /// The source address stays part of the identity: the same PGN sent by two
    /// nodes can be two different DBC messages. Keys are `(pgn << 8) | sa`.
    /// Rebuilt on load, never serialized.
    #[serde(skip)]
    pgn_index: HashMap<u32, u32>,
    /// All CAN network nodes (`BU_`) declared in the DBC, including ones that
    /// transmit no messages. `#[serde(default)]` so projects saved before this
    /// field existed still deserialize.
    #[serde(default)]
    pub nodes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedMessage {
    pub id: u32,
    pub name: String,
    pub dlc: u64,
    /// 29-bit (extended) frame format, from bit 31 of the DBC's raw id. Kept
    /// separate from the id value: a 29-bit message whose id happens to be
    /// ≤ 0x7FF is legal and must still be sent as an extended frame.
    #[serde(default)]
    pub is_extended: bool,
    pub signals: Vec<ParsedSignal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transmitter: Option<String>,
    /// `GenMsgCycleTime` attribute (`BA_`): the intended transmit period. A
    /// value of 0 in the DBC means "not cyclic" and is stored as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycle_time_ms: Option<u32>,
    /// `CM_ BO_` comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedSignal {
    pub name: String,
    pub message_id: u32,
    pub message_name: String,
    pub start_bit: u64,
    pub length: u64,
    pub little_endian: bool,
    pub signed: bool,
    pub factor: f64,
    pub offset: f64,
    pub min: f64,
    pub max: f64,
    pub unit: String,
    /// DBC `VAL_` value descriptions (enum entries). Empty when the signal has none.
    #[serde(default)]
    pub enum_values: Vec<SignalEnumValue>,
    /// True for a multiplexor switch signal (`M`, or the switch half of `m<v>M`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub multiplexor: bool,
    /// For multiplexed signals (`m<v>`): the switch value this signal is active
    /// for. `None` for plain signals and the top-level switch. Extended
    /// multiplexing (`SG_MUL_VAL_`) is not interpreted; such signals gate on
    /// their `m<v>` value against the top-level switch like plain mux.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mux_value: Option<i64>,
    /// IEEE float width from `SIG_VALTYPE_`: 32 or 64. `None` = plain integer
    /// signal. Float signals reinterpret the extracted bits as f32/f64 before
    /// factor/offset scaling instead of treating them as an integer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub float_bits: Option<u8>,
    /// `GenSigStartValue` attribute (`BA_`), converted from the raw value the
    /// DBC stores to a physical value (× factor + offset) so consumers can use
    /// it directly as an initial signal value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_value: Option<f64>,
    /// `CM_ SG_` comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalEnumValue {
    /// Raw signal value this description applies to.
    pub value: i64,
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecodedCanSignal {
    pub name: String,
    pub physical: f64,
    /// Raw signal value, sign-extended for signed signals. This is what DBC VAL_
    /// tables map to labels, so it is what the frontend must key enum lookups off.
    pub raw: i64,
    pub unit: String,
    /// False when the signal belongs to a multiplexer group other than the one
    /// selected by this frame's switch value. Inactive entries are placeholders
    /// (empty name/unit, NaN physical) kept only so the decoded list stays
    /// index-aligned with the message's signal order.
    pub active: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecodedCanMessage {
    pub name: String,
    pub signals: Vec<DecodedCanSignal>,
}

impl ParsedMessage {
    pub fn decode_frame(&self, frame: &CanFrame) -> Result<DecodedCanMessage, String> {
        if frame.can_id != self.id {
            return Err(format!("Frame CAN ID {} does not match message ID {}", frame.can_id, self.id));
        }
        Ok(self.decode_data(&frame.data))
    }

    /// The top-level multiplexor switch signal, if this message is multiplexed.
    /// A switch that is itself multiplexed (`m<v>M`, extended multiplexing) is
    /// only used as a fallback when no plain `M` switch exists.
    pub fn multiplexor(&self) -> Option<&ParsedSignal> {
        self.signals
            .iter()
            .find(|s| s.multiplexor && s.mux_value.is_none())
            .or_else(|| self.signals.iter().find(|s| s.multiplexor))
    }

    /// Decode this message's signals out of a raw data buffer, without any CAN
    /// id check (J1939 frames match by PGN, not by exact id). Signals of
    /// inactive multiplexer groups are returned as inactive placeholders so the
    /// result stays index-aligned with `self.signals`.
    pub fn decode_data(&self, data: &[u8]) -> DecodedCanMessage {
        // Multiplexed messages: decode the switch first — signals of inactive
        // groups share bit positions, so extracting them would yield garbage.
        let mux_raw = self.multiplexor().map(|m| {
            raw_signed(
                extract_bits(data, m.start_bit, m.length, m.little_endian),
                m.length,
                m.signed,
            )
        });

        let mut decoded_signals = Vec::new();
        for sig in &self.signals {
            let active = match sig.mux_value {
                None => true,
                Some(v) => mux_raw == Some(v),
            };
            if !active {
                decoded_signals.push(DecodedCanSignal {
                    name: String::new(),
                    physical: f64::NAN,
                    raw: 0,
                    unit: String::new(),
                    active: false,
                });
                continue;
            }
            let bits = extract_bits(data, sig.start_bit, sig.length, sig.little_endian);
            let (physical, raw) = if let Some(width) = sig.float_bits {
                // SIG_VALTYPE_ float/double: the bit pattern IS an IEEE float,
                // not an integer. Reinterpret, then scale. There is no integer
                // raw value; report the unscaled float rounded so raw readouts
                // show something meaningful (float→int casts saturate).
                let base = if width == 32 {
                    f32::from_bits(bits as u32) as f64
                } else {
                    f64::from_bits(bits)
                };
                (base * sig.factor + sig.offset, base.round() as i64)
            } else {
                (
                    apply_scaling(bits, sig.length, sig.signed, sig.factor, sig.offset),
                    raw_signed(bits, sig.length, sig.signed),
                )
            };
            decoded_signals.push(DecodedCanSignal {
                name: sig.name.clone(),
                physical,
                raw,
                unit: sig.unit.clone(),
                active: true,
            });
        }
        DecodedCanMessage {
            name: self.name.clone(),
            signals: decoded_signals,
        }
    }

    /// Encode the given signal values into a buffer sized to this message's DLC.
    /// For multiplexed messages only the switch, plain signals, and the group
    /// selected by the provided switch value are encoded — inactive groups
    /// share bits and would overwrite each other.
    pub fn encode_signals(&self, signal_values: &HashMap<String, f64>) -> Vec<u8> {
        let mut buf = vec![0u8; self.dlc as usize];
        let mux_raw = self.multiplexor().map(|m| {
            let v = signal_values.get(&m.name).copied().unwrap_or(0.0);
            if m.factor == 0.0 {
                0
            } else {
                ((v - m.offset) / m.factor).round() as i64
            }
        });
        for sig in &self.signals {
            let active = match sig.mux_value {
                None => true,
                Some(v) => mux_raw == Some(v),
            };
            if !active {
                continue;
            }
            if let Some(&v) = signal_values.get(&sig.name) {
                if let Some(width) = sig.float_bits {
                    // Float signal: unscale to the IEEE base value and pack its
                    // bit pattern — no integer rounding anywhere.
                    let base = if sig.factor == 0.0 { 0.0 } else { (v - sig.offset) / sig.factor };
                    let bits = if width == 32 {
                        f32::to_bits(base as f32) as u64
                    } else {
                        f64::to_bits(base)
                    };
                    pack_bits(&mut buf, bits, sig.start_bit, sig.length, sig.little_endian);
                } else {
                    encode(&mut buf, v, sig.start_bit, sig.length, sig.little_endian, sig.factor, sig.offset);
                }
            }
        }
        buf
    }
}

impl ParsedDbc {
    pub fn new(path: &str) -> Result<Self, String> {
        let mut dbc = Self {
            path: path.to_string(),
            messages: HashMap::new(),
            pgn_index: HashMap::new(),
            nodes: Vec::new(),
        };
        dbc.reload()?;

        Ok(dbc)
    }

    pub fn reload(&mut self) -> Result<(), String> {
        let text = std::fs::read_to_string(self.path.as_str()).map_err(|e| format!("Failed to read DBC file: {e}"))?;
        let dbc = Dbc::try_from(text.as_str()).map_err(|e| format!("Failed to parse DBC: {e}"))?;

        // Attribute/comment lookups prebuilt once: the crate only offers linear
        // scans, which would make populating every signal O(n²) on large DBCs.
        let msg_cycle_times: HashMap<MessageId, f64> = dbc
            .attribute_values_message
            .iter()
            .filter(|a| a.name == "GenMsgCycleTime")
            .filter_map(|a| attribute_to_f64(&a.value).map(|v| (a.message_id, v)))
            .collect();
        let sig_start_values: HashMap<(MessageId, &str), f64> = dbc
            .attribute_values_signal
            .iter()
            .filter(|a| a.name == "GenSigStartValue")
            .filter_map(|a| attribute_to_f64(&a.value).map(|v| ((a.message_id, a.signal_name.as_str()), v)))
            .collect();
        let mut msg_comments: HashMap<MessageId, &str> = HashMap::new();
        let mut sig_comments: HashMap<(MessageId, &str), &str> = HashMap::new();
        for c in &dbc.comments {
            match c {
                Comment::Message { id, comment } => {
                    msg_comments.insert(*id, comment.as_str());
                }
                Comment::Signal { message_id, name, comment } => {
                    sig_comments.insert((*message_id, name.as_str()), comment.as_str());
                }
                _ => {}
            }
        }

        let mut messages = HashMap::new();
        for msg in &dbc.messages {
            let (raw_id, is_extended) = match msg.id {
                MessageId::Standard(id) => (u32::from(id), false),
                MessageId::Extended(id) => (id, true),
            };
            let msg_name = msg.name.clone();

            let signals = msg
                .signals
                .iter()
                .map(|sig| {
                    let enum_values = dbc
                        .value_descriptions_for_signal(msg.id, &sig.name)
                        .map(|descs| {
                            descs
                                .iter()
                                .map(|d| SignalEnumValue {
                                    value: d.id,
                                    description: d.description.clone(),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let (multiplexor, mux_value) = match sig.multiplexer_indicator {
                        MultiplexIndicator::Multiplexor => (true, None),
                        MultiplexIndicator::MultiplexedSignal(v) => (false, Some(v as i64)),
                        MultiplexIndicator::MultiplexorAndMultiplexedSignal(v) => (true, Some(v as i64)),
                        MultiplexIndicator::Plain => (false, None),
                    };
                    let float_bits = match dbc.extended_value_type_for_signal(msg.id, &sig.name) {
                        Some(SignalExtendedValueType::IEEEfloat32Bit) => Some(32),
                        Some(SignalExtendedValueType::IEEEdouble64bit) => Some(64),
                        _ => None,
                    };
                    // GenSigStartValue is a raw value (also for float signals,
                    // where "raw" is the IEEE base value) — scale it to physical.
                    let start_value = sig_start_values
                        .get(&(msg.id, sig.name.as_str()))
                        .map(|raw| raw * sig.factor + sig.offset);
                    ParsedSignal {
                        name: sig.name.clone(),
                        message_id: raw_id,
                        message_name: msg_name.clone(),
                        start_bit: sig.start_bit,
                        length: sig.size,
                        little_endian: sig.byte_order == ByteOrder::LittleEndian,
                        signed: sig.value_type == ValueType::Signed,
                        factor: sig.factor,
                        offset: sig.offset,
                        min: numeric_to_f64(sig.min),
                        max: numeric_to_f64(sig.max),
                        unit: sig.unit.clone(),
                        enum_values,
                        multiplexor,
                        mux_value,
                        float_bits,
                        start_value,
                        comment: sig_comments.get(&(msg.id, sig.name.as_str())).map(|c| c.to_string()),
                    }
                })
                .collect();

            let transmitter = match &msg.transmitter {
                can_dbc::Transmitter::NodeName(n) => Some(n.clone()),
                can_dbc::Transmitter::VectorXXX => None,
            };

            messages.insert(
                raw_id,
                ParsedMessage {
                    id: raw_id,
                    name: msg_name,
                    dlc: msg.size,
                    is_extended,
                    signals,
                    transmitter,
                    // Cycle time 0 means "not cyclic" per convention — treat as absent.
                    cycle_time_ms: msg_cycle_times
                        .get(&msg.id)
                        .filter(|&&v| v > 0.0)
                        .map(|&v| v.round() as u32),
                    comment: msg_comments.get(&msg.id).map(|c| c.to_string()),
                },
            );
        }

        self.pgn_index = messages
            .values()
            .filter(|m| m.is_extended)
            .map(|m| {
                let j = crate::j1939::decode_id(m.id);
                ((j.pgn << 8) | j.sa as u32, m.id)
            })
            .collect();
        self.messages = messages;
        self.nodes = dbc.nodes.iter().map(|n| n.0.clone()).collect();

        Ok(())
    }

    /// Decode a raw frame against the matching message, if one exists.
    pub fn decode_frame(&self, frame: &CanFrame) -> Option<DecodedCanMessage> {
        self.messages.get(&frame.can_id).and_then(|msg| msg.decode_frame(frame).ok())
    }

    /// Decode a frame on a J1939 channel: exact id match first, then by
    /// (PGN, source address) — ignoring only the priority bits and, for PDU1
    /// groups, the destination address. Returns the decoded message and the
    /// DBC message id it matched, which callers store so signal-history
    /// queries keyed on the DBC id can find these frames.
    pub fn decode_frame_j1939(&self, frame: &CanFrame) -> Option<(DecodedCanMessage, u32)> {
        if let Some(msg) = self.messages.get(&frame.can_id) {
            return Some((msg.decode_data(&frame.data), msg.id));
        }
        if !frame.is_extended {
            return None;
        }
        let j = crate::j1939::decode_id(frame.can_id);
        let key = (j.pgn << 8) | j.sa as u32;
        let msg = self.pgn_index.get(&key).and_then(|id| self.messages.get(id))?;
        Some((msg.decode_data(&frame.data), msg.id))
    }

}

fn encode(data: &mut [u8], value: f64, start_bit: u64, length: u64, little_endian: bool, factor: f64, offset: f64) {
    // A degenerate factor of 0 means every raw value encodes the same physical
    // (the offset) — use raw 0 rather than dividing to ±inf and saturating.
    let raw = if factor == 0.0 {
        0
    } else {
        ((value - offset) / factor).round() as i64
    };
    let mask = if length >= 64 { u64::MAX } else { (1u64 << length) - 1 };
    let raw_u64 = (raw as u64) & mask;
    pack_bits(data, raw_u64, start_bit, length, little_endian);
}

/// Sign-extend a raw bit pattern into a signed integer for signed signals. DBC
/// VAL_ tables map the signal's (signed) raw value, so this is the value enum
/// lookups must compare against.
fn raw_signed(raw: u64, length: u64, signed: bool) -> i64 {
    if signed && length > 0 && length < 64 {
        let msb_mask = 1u64 << (length - 1);
        if raw & msb_mask != 0 {
            return (raw | !((1u64 << length) - 1)) as i64;
        }
    }
    raw as i64
}

fn apply_scaling(raw: u64, length: u64, signed: bool, factor: f64, offset: f64) -> f64 {
    let physical = if signed && length > 0 {
        let msb_mask = 1u64 << (length - 1);
        if raw & msb_mask != 0 {
            let sign_extended = raw | !((1u64 << length) - 1);
            sign_extended as i64 as f64
        } else {
            raw as f64
        }
    } else {
        raw as f64
    };
    physical * factor + offset
}

fn extract_bits(data: &[u8], start_bit: u64, length: u64, little_endian: bool) -> u64 {
    let mut raw = 0u64;
    if little_endian {
        for i in 0..length {
            let bit_pos = start_bit + i;
            let byte_idx = (bit_pos / 8) as usize;
            let bit_in_byte = (bit_pos % 8) as u32;
            if byte_idx < data.len() {
                raw |= (((data[byte_idx] >> bit_in_byte) & 1) as u64) << i;
            }
        }
    } else {
        // Motorola/Big-endian: start_bit is MSB in DBC bit numbering
        let mut bit_pos = start_bit;
        for i in 0..length {
            let byte_idx = (bit_pos / 8) as usize;
            let bit_in_byte = (bit_pos % 8) as u32;
            if byte_idx < data.len() {
                raw |= (((data[byte_idx] >> bit_in_byte) & 1) as u64) << (length - 1 - i);
            }
            if bit_pos % 8 == 0 {
                bit_pos = bit_pos.saturating_add(15);
            } else {
                bit_pos -= 1;
            }
        }
    }
    raw
}

/// Write the low `length` bits of `raw` into the buffer at the signal's bit
/// position. Also used by the sim generator to stamp counters/checksums into an
/// already-encoded frame.
pub(crate) fn pack_bits(data: &mut [u8], raw: u64, start_bit: u64, length: u64, little_endian: bool) {
    if little_endian {
        for i in 0..length {
            let bit_pos = start_bit + i;
            let byte_idx = (bit_pos / 8) as usize;
            let bit_in_byte = (bit_pos % 8) as u32;
            if byte_idx < data.len() {
                if (raw >> i) & 1 == 1 {
                    data[byte_idx] |= 1 << bit_in_byte;
                } else {
                    data[byte_idx] &= !(1u8 << bit_in_byte);
                }
            }
        }
    } else {
        let mut bit_pos = start_bit;
        for i in 0..length {
            let byte_idx = (bit_pos / 8) as usize;
            let bit_in_byte = (bit_pos % 8) as u32;
            if byte_idx < data.len() {
                if (raw >> (length - 1 - i)) & 1 == 1 {
                    data[byte_idx] |= 1 << bit_in_byte;
                } else {
                    data[byte_idx] &= !(1u8 << bit_in_byte);
                }
            }
            if bit_pos % 8 == 0 {
                bit_pos = bit_pos.saturating_add(15);
            } else {
                bit_pos -= 1;
            }
        }
    }
}

/// Numeric view of a `BA_` attribute value. DBC tools disagree on whether
/// numeric attributes are written quoted or bare, so parse strings too.
fn attribute_to_f64(v: &AttributeValue) -> Option<f64> {
    match v {
        AttributeValue::Uint(n) => Some(*n as f64),
        AttributeValue::Int(n) => Some(*n as f64),
        AttributeValue::Double(n) => Some(*n),
        AttributeValue::String(s) => s.trim().parse().ok(),
    }
}

fn numeric_to_f64(v: NumericValue) -> f64 {
    match v {
        NumericValue::Uint(n) => n as f64,
        NumericValue::Int(n) => n as f64,
        NumericValue::Double(n) => n,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a minimal J1939 DBC (one message, PGN 0xF004, SA 0x00) written to
    /// a temp file, since ParsedDbc only constructs from disk.
    fn j1939_dbc() -> ParsedDbc {
        let path = std::env::temp_dir().join("canvaz_test_pgn_sa.dbc");
        // BO_ id: 0x0CF00400 (prio 3, PGN F004, SA 00) with the DBC extended-id
        // flag (bit 31) set.
        let content = format!(
            "VERSION \"\"\n\nNS_ :\n\nBS_:\n\nBU_:\n\nBO_ {} EEC1: 8 Vector__XXX\n SG_ EngSpeed : 24|16@1+ (0.125,0) [0|8031.875] \"rpm\" Vector__XXX\n",
            0x8CF00400u32
        );
        std::fs::write(&path, content).expect("write temp dbc");
        ParsedDbc::new(path.to_str().unwrap()).expect("parse dbc")
    }

    fn frame(can_id: u32) -> CanFrame {
        CanFrame {
            can_id,
            is_extended: true,
            data: vec![0; 8],
            timestamp_ms: None,
        }
    }

    /// Parse a minimal multiplexed DBC: an 8-bit switch, one 16-bit signal per
    /// mux group 0/1 sharing bits 8–23, and a plain signal.
    fn mux_dbc() -> ParsedDbc {
        let path = std::env::temp_dir().join("canvaz_test_mux.dbc");
        let content = "VERSION \"\"\n\nNS_ :\n\nBS_:\n\nBU_:\n\nBO_ 512 MuxMsg: 8 Vector__XXX\n \
             SG_ Mode M : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n \
             SG_ SigA m0 : 8|16@1+ (1,0) [0|65535] \"\" Vector__XXX\n \
             SG_ SigB m1 : 8|16@1+ (0.5,0) [0|100] \"\" Vector__XXX\n \
             SG_ Always : 24|8@1+ (1,0) [0|255] \"\" Vector__XXX\n";
        std::fs::write(&path, content).expect("write temp dbc");
        ParsedDbc::new(path.to_str().unwrap()).expect("parse dbc")
    }

    #[test]
    fn extended_flag_comes_from_dbc_bit31_not_id_value() {
        let path = std::env::temp_dir().join("canvaz_test_extid.dbc");
        // BO_ 0x80000123: extended-id flag (bit 31) set with a 29-bit id of
        // 0x123 — an id that would pass for standard if inferred from its value.
        let content = format!(
            "VERSION \"\"\n\nNS_ :\n\nBS_:\n\nBU_:\n\nBO_ {} ExtLow: 8 Vector__XXX\n \
             SG_ A : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n\nBO_ 768 Std: 8 Vector__XXX\n \
             SG_ B : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n",
            0x8000_0123u32
        );
        std::fs::write(&path, content).expect("write temp dbc");
        let dbc = ParsedDbc::new(path.to_str().unwrap()).expect("parse dbc");
        let ext = dbc.messages.get(&0x123).expect("extended message keyed by its 29-bit id");
        assert!(ext.is_extended, "29-bit message with id ≤ 0x7FF must keep the extended flag");
        assert!(!dbc.messages.get(&768).expect("standard message").is_extended);
    }

    /// One float32 signal (factor 2, offset 10) and one double64 signal in
    /// separate messages, declared via SIG_VALTYPE_.
    fn float_dbc() -> ParsedDbc {
        let path = std::env::temp_dir().join("canvaz_test_float.dbc");
        let content = "VERSION \"\"\n\nNS_ :\n\nBS_:\n\nBU_:\n\nBO_ 512 FMsg: 8 Vector__XXX\n \
             SG_ F32 : 0|32@1- (2,10) [0|0] \"V\" Vector__XXX\n\nBO_ 513 DMsg: 8 Vector__XXX\n \
             SG_ F64 : 0|64@1- (1,0) [0|0] \"\" Vector__XXX\n\n\
             SIG_VALTYPE_ 512 F32 : 1;\nSIG_VALTYPE_ 513 F64 : 2;\n";
        std::fs::write(&path, content).expect("write temp dbc");
        ParsedDbc::new(path.to_str().unwrap()).expect("parse dbc")
    }

    #[test]
    fn float_signals_decode_as_ieee_not_integer() {
        let dbc = float_dbc();

        // f32 1.5 = 0x3FC00000, little-endian bytes [00 00 C0 3F].
        // physical = 1.5 × 2 + 10 = 13. Integer decode of the same bytes would
        // give 0x3FC00000 × 2 + 10 — wildly wrong.
        let msg = dbc.messages.get(&512).expect("F32 message");
        assert_eq!(msg.signals[0].float_bits, Some(32));
        let d = msg.decode_data(&[0x00, 0x00, 0xC0, 0x3F, 0, 0, 0, 0]);
        assert_eq!(d.signals[0].physical, 13.0);

        // f64 2.5 = 0x4004000000000000, little-endian bytes.
        let msg = dbc.messages.get(&513).expect("F64 message");
        assert_eq!(msg.signals[0].float_bits, Some(64));
        let d = msg.decode_data(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x40]);
        assert_eq!(d.signals[0].physical, 2.5);
    }

    #[test]
    fn float_signals_encode_roundtrip() {
        let dbc = float_dbc();
        let msg = dbc.messages.get(&512).expect("F32 message");
        let values: HashMap<String, f64> = [("F32".to_string(), 13.0)].into();
        let buf = msg.encode_signals(&values);
        assert_eq!(&buf[..4], &[0x00, 0x00, 0xC0, 0x3F], "13 unscales to f32 1.5");
        assert_eq!(msg.decode_data(&buf).signals[0].physical, 13.0);

        // Fractional physical values must survive without integer quantization.
        let msg = dbc.messages.get(&513).expect("F64 message");
        let values: HashMap<String, f64> = [("F64".to_string(), 0.123456789)].into();
        let buf = msg.encode_signals(&values);
        assert_eq!(msg.decode_data(&buf).signals[0].physical, 0.123456789);
    }

    #[test]
    fn attributes_and_comments_are_parsed() {
        let path = std::env::temp_dir().join("canvaz_test_attrs.dbc");
        // Temp signal: raw start 70 with (0.5, -40) → physical -5.
        // NoCycle has GenMsgCycleTime 0, which means "not cyclic" → None.
        let content = "VERSION \"\"\n\nNS_ :\n\nBS_:\n\nBU_:\n\nBO_ 512 Status: 8 Vector__XXX\n \
             SG_ Temp : 0|8@1+ (0.5,-40) [-40|87.5] \"degC\" Vector__XXX\n \
             SG_ Plain : 8|8@1+ (1,0) [0|255] \"\" Vector__XXX\n\n\
             BO_ 513 NoCycle: 8 Vector__XXX\n \
             SG_ X : 0|8@1+ (1,0) [0|255] \"\" Vector__XXX\n\n\
             CM_ BO_ 512 \"Periodic status frame\";\n\
             CM_ SG_ 512 Temp \"Coolant temperature\";\n\
             BA_DEF_ BO_ \"GenMsgCycleTime\" INT 0 60000;\n\
             BA_DEF_ SG_ \"GenSigStartValue\" INT 0 255;\n\
             BA_DEF_DEF_ \"GenMsgCycleTime\" 0;\n\
             BA_DEF_DEF_ \"GenSigStartValue\" 0;\n\
             BA_ \"GenMsgCycleTime\" BO_ 512 250;\n\
             BA_ \"GenMsgCycleTime\" BO_ 513 0;\n\
             BA_ \"GenSigStartValue\" SG_ 512 Temp 70;\n";
        std::fs::write(&path, content).expect("write temp dbc");
        let dbc = ParsedDbc::new(path.to_str().unwrap()).expect("parse dbc");

        let msg = dbc.messages.get(&512).expect("Status message");
        assert_eq!(msg.cycle_time_ms, Some(250));
        assert_eq!(msg.comment.as_deref(), Some("Periodic status frame"));
        let temp = &msg.signals[0];
        assert_eq!(temp.start_value, Some(-5.0), "raw 70 × 0.5 − 40");
        assert_eq!(temp.comment.as_deref(), Some("Coolant temperature"));
        let plain = &msg.signals[1];
        assert_eq!(plain.start_value, None, "no GenSigStartValue set");
        assert_eq!(plain.comment, None);

        let no_cycle = dbc.messages.get(&513).expect("NoCycle message");
        assert_eq!(no_cycle.cycle_time_ms, None, "cycle time 0 means not cyclic");
        assert_eq!(no_cycle.comment, None);
    }

    #[test]
    fn mux_decode_gates_on_switch_value() {
        let dbc = mux_dbc();
        let msg = dbc.messages.get(&512).expect("message");

        // Mode = 0: SigA active, SigB inactive.
        let d = msg.decode_data(&[0x00, 0x34, 0x12, 0x07, 0, 0, 0, 0]);
        let by_idx = &d.signals;
        assert_eq!(by_idx.len(), 4, "one entry per DBC signal, placeholders included");
        assert!(by_idx[0].active && by_idx[0].raw == 0, "switch");
        assert!(by_idx[1].active, "SigA active for Mode=0");
        assert_eq!(by_idx[1].raw, 0x1234);
        assert!(!by_idx[2].active, "SigB inactive for Mode=0");
        assert!(by_idx[2].physical.is_nan());
        assert!(by_idx[3].active && by_idx[3].raw == 7, "plain signal always active");

        // Mode = 1: SigB active with its own scaling, SigA inactive.
        let d = msg.decode_data(&[0x01, 0x14, 0x00, 0x00, 0, 0, 0, 0]);
        assert!(!d.signals[1].active, "SigA inactive for Mode=1");
        assert!(d.signals[2].active, "SigB active for Mode=1");
        assert_eq!(d.signals[2].physical, 10.0, "raw 20 × factor 0.5");
    }

    #[test]
    fn mux_encode_skips_inactive_group() {
        let dbc = mux_dbc();
        let msg = dbc.messages.get(&512).expect("message");
        let values: HashMap<String, f64> = [
            ("Mode".to_string(), 1.0),
            ("SigA".to_string(), 999.0), // inactive — must not reach the buffer
            ("SigB".to_string(), 10.0),  // raw 20
            ("Always".to_string(), 5.0),
        ]
        .into();
        let buf = msg.encode_signals(&values);
        assert_eq!(buf, vec![0x01, 20, 0x00, 5, 0, 0, 0, 0]);
    }

    #[test]
    fn j1939_match_ignores_priority_but_not_source_address() {
        let dbc = j1939_dbc();
        // Same PGN and SA, different priority (6 instead of 3): must match.
        let hit = dbc.decode_frame_j1939(&frame(0x18F00400));
        assert!(hit.is_some(), "same PGN+SA with different priority should match");
        assert_eq!(hit.unwrap().0.name, "EEC1");
        // Same PGN from a different source address: must NOT match.
        assert!(
            dbc.decode_frame_j1939(&frame(0x18F00417)).is_none(),
            "same PGN from another source address must not match"
        );
    }
}
