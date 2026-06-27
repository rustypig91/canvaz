use std::collections::HashMap;

use can_dbc::{ByteOrder, Dbc, MessageId, NumericValue, ValueType};
use serde::{Deserialize, Serialize};

use crate::can_frame::{CanSignal, CanFrame, DecodedCanMessage};


#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedDbc {
    pub path: String,
    pub messages: HashMap<u32, ParsedMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedMessage {
    pub id: u32,
    pub name: String,
    pub dlc: u64,
    pub signals: Vec<ParsedSignal>,
}

impl ParsedMessage {
    pub fn decode_frame(&self, frame: &CanFrame) -> Result<DecodedCanMessage, String> {
        if frame.can_id != self.id {
            return Err(format!(
                "Frame CAN ID {} does not match message ID {}",
                frame.can_id, self.id
            ));
        }

        let mut decoded_signals = Vec::new();
        for sig in &self.signals {
            let value = crate::signal_codec::decode(
                &frame.data,
                sig.start_bit,
                sig.length,
                sig.little_endian,
                sig.signed,
                sig.factor,
                sig.offset,
            );
            decoded_signals.push(CanSignal {
                name: sig.name.clone(),
                physical: value,
                raw: 0,
                dlc: 0,
                signals: Vec::new(),
            });
        }
        Ok(DecodedCanMessage {
            name: self.name.clone(),
            signals: decoded_signals,
        })
    }
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
}


fn numeric_to_f64(v: NumericValue) -> f64 {
    match v {
        NumericValue::Uint(n) => n as f64,
        NumericValue::Int(n) => n as f64,
        NumericValue::Double(n) => n,
    }
}

#[allow(dead_code)]
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
                .map(|sig| ParsedSignal {
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
                })
                .collect();

            messages.insert(raw_id, ParsedMessage {
                id: raw_id,
                name: msg_name,
                dlc: msg.size,
                signals,
            });
        }

        self.messages = messages;

        Ok(())
    }

    pub fn find_signal(&self, signal_name: &str) -> Option<&ParsedSignal> {
        self.messages
            .values()
            .flat_map(|m| m.signals.iter())
            .find(|s| s.name == signal_name)
    }

    pub fn find_message(&self, frame: &CanFrame) -> Option<&ParsedMessage> {
        self.messages.get(&frame.can_id)
    }

    pub fn parse_frame(&self, frame: &CanFrame) -> Option<DecodedCanMessage> {
        self.find_message(frame)
            .and_then(|msg| msg.decode_frame(frame).ok())
    }
}
