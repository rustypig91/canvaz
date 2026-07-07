use std::collections::HashMap;

use can_dbc::{ByteOrder, Dbc, MessageId, NumericValue, ValueType};
use serde::{Deserialize, Serialize};

use crate::can_communication::CanFrame;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedDbc {
    pub path: String,
    /// Keyed by CAN id. Serializes to a JSON object; the frontend treats it as a map.
    pub messages: HashMap<u32, ParsedMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedMessage {
    pub id: u32,
    pub name: String,
    pub dlc: u64,
    pub signals: Vec<ParsedSignal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transmitter: Option<String>,
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

        let mut decoded_signals = Vec::new();
        for sig in &self.signals {
            let raw = raw_signed(
                extract_bits(&frame.data, sig.start_bit, sig.length, sig.little_endian),
                sig.length,
                sig.signed,
            );
            let physical = decode(
                &frame.data,
                sig.start_bit,
                sig.length,
                sig.little_endian,
                sig.signed,
                sig.factor,
                sig.offset,
            );
            decoded_signals.push(DecodedCanSignal {
                name: sig.name.clone(),
                physical,
                raw,
                unit: sig.unit.clone(),
            });
        }
        Ok(DecodedCanMessage {
            name: self.name.clone(),
            signals: decoded_signals,
        })
    }

    /// Encode the given signal values into a buffer sized to this message's DLC.
    pub fn encode_signals(&self, signal_values: &HashMap<String, f64>) -> Vec<u8> {
        let mut buf = vec![0u8; self.dlc as usize];
        for sig in &self.signals {
            if let Some(&v) = signal_values.get(&sig.name) {
                encode(&mut buf, v, sig.start_bit, sig.length, sig.little_endian, sig.factor, sig.offset);
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
        };
        dbc.reload()?;

        Ok(dbc)
    }

    pub fn reload(&mut self) -> Result<(), String> {
        let text = std::fs::read_to_string(self.path.as_str()).map_err(|e| format!("Failed to read DBC file: {e}"))?;
        let dbc = Dbc::try_from(text.as_str()).map_err(|e| format!("Failed to parse DBC: {e}"))?;

        let mut messages = HashMap::new();
        for msg in &dbc.messages {
            let raw_id = match msg.id {
                MessageId::Standard(id) => u32::from(id),
                MessageId::Extended(id) => id,
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
                    signals,
                    transmitter,
                },
            );
        }

        self.messages = messages;

        Ok(())
    }

    /// Decode a raw frame against the matching message, if one exists.
    pub fn decode_frame(&self, frame: &CanFrame) -> Option<DecodedCanMessage> {
        self.messages.get(&frame.can_id).and_then(|msg| msg.decode_frame(frame).ok())
    }

    /// Encode signal values for `msg_id` into a data buffer.
    pub fn encode_message(&self, msg_id: u32, signal_values: &HashMap<String, f64>) -> Result<Vec<u8>, String> {
        self.messages
            .get(&msg_id)
            .map(|msg| msg.encode_signals(signal_values))
            .ok_or_else(|| format!("Message 0x{:X} not in DBC", msg_id))
    }
}

fn decode(data: &[u8], start_bit: u64, length: u64, little_endian: bool, signed: bool, factor: f64, offset: f64) -> f64 {
    let raw = extract_bits(data, start_bit, length, little_endian);
    apply_scaling(raw, length, signed, factor, offset)
}

fn encode(data: &mut [u8], value: f64, start_bit: u64, length: u64, little_endian: bool, factor: f64, offset: f64) {
    // A degenerate factor of 0 means every raw value encodes the same physical
    // (the offset) — use raw 0 rather than dividing to ±inf and saturating.
    let raw = if factor == 0.0 { 0 } else { ((value - offset) / factor).round() as i64 };
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

fn pack_bits(data: &mut [u8], raw: u64, start_bit: u64, length: u64, little_endian: bool) {
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

fn numeric_to_f64(v: NumericValue) -> f64 {
    match v {
        NumericValue::Uint(n) => n as f64,
        NumericValue::Int(n) => n as f64,
        NumericValue::Double(n) => n,
    }
}
