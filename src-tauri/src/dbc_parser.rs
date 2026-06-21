use can_dbc::{ByteOrder, Dbc, MessageId, NumericValue, ValueType};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedDbc {
    pub path: String,
    pub messages: Vec<ParsedMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedMessage {
    pub id: u32,
    pub name: String,
    pub dlc: u64,
    pub signals: Vec<ParsedSignal>,
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

pub fn parse_dbc(path: &str) -> Result<ParsedDbc, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Failed to read DBC file: {e}"))?;

    // DBC files may be UTF-8 or Windows-1252 encoded
    let text = match String::from_utf8(bytes.clone()) {
        Ok(s) => s,
        Err(_) => can_dbc::decode_cp1252(&bytes)
            .map(|cow| cow.into_owned())
            .ok_or("Failed to decode DBC file as UTF-8 or CP1252")?,
    };

    let dbc = Dbc::try_from(text.as_str())
        .map_err(|e| format!("Failed to parse DBC: {e}"))?;

    let mut messages = Vec::new();
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

        messages.push(ParsedMessage {
            id: raw_id,
            name: msg_name,
            dlc: msg.size,
            signals,
        });
    }

    messages.sort_by_key(|m| m.id);

    Ok(ParsedDbc {
        path: path.to_string(),
        messages,
    })
}

#[allow(dead_code)]
impl ParsedDbc {
    pub fn find_signal(&self, signal_name: &str) -> Option<&ParsedSignal> {
        self.messages
            .iter()
            .flat_map(|m| m.signals.iter())
            .find(|s| s.name == signal_name)
    }

    pub fn signals_for_message(&self, can_id: u32) -> Option<&[ParsedSignal]> {
        self.messages
            .iter()
            .find(|m| m.id == can_id)
            .map(|m| m.signals.as_slice())
    }
}
