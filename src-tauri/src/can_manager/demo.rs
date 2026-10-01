//! Deterministic, DBC-encoded traffic for the compile-time demo feature.
use super::*;

impl CanManager {
    pub fn seed_demo(&mut self, dbc: ParsedDbc, start_ms: u64) {
        self.shared.lock().unwrap().channels.insert(1, demo_channel(dbc, start_ms));
    }
}

fn demo_channel(dbc: ParsedDbc, start_ms: u64) -> ChannelData {
    let mut channel = ChannelData::new(ChannelInfo {
        backend: "CAN".into(),
        name: "Powertrain CAN".into(),
    });
    channel.bitrate = 500_000;
    channel.listen_only = true;
    channel.dbc = Some(Arc::new(dbc.clone()));
    for tick in 0..=300 {
        let t = tick as f64 / 10.0;
        let wave = (t * 0.55).sin();
        let rpm = 2400.0 + 1000.0 * wave + 180.0 * (t * 1.4).sin();
        let values = HashMap::from([
            ("EngineRPM".into(), rpm),
            ("CoolantTemp".into(), 86.0 + 3.0 * (t * 0.12).sin()),
            ("GearState".into(), 5.0),
            ("EngineState".into(), 2.0),
            ("ThrottlePercent".into(), 45.0 + 25.0 * wave),
            ("VehicleSpeed".into(), 72.0 + 18.0 * wave),
            ("IgnitionState".into(), 2.0),
            ("DoorStatus".into(), 0.0),
            ("HeadlightMode".into(), 4.0),
            ("AmbientTemp".into(), 21.5),
            ("RequestedGear".into(), 3.0),
            ("TurnSignal".into(), 0.0),
            ("WiperSpeed".into(), 0.0),
        ]);
        for id in [256, 512, 768] {
            let message = &dbc.messages[&id];
            let data = message.encode_signals(&values);
            let decoded = message.decode_data(&data);
            channel.frames.push_back(StoredFrame {
                can_id: id,
                is_extended: false,
                data,
                timestamp_ms: start_ms + tick * 100,
                direction: if id == 768 { "tx" } else { "rx" },
                message_name: Some(decoded.name),
                j1939: None,
                reassembled: false,
                error: None,
                dbc_msg_id: Some(id),
                signals: decoded
                    .signals
                    .into_iter()
                    .filter(|s| s.active)
                    .map(|s| StoredSignal {
                        name: s.name,
                        value: s.physical,
                        raw: s.raw,
                        unit: s.unit,
                    })
                    .collect(),
            });
        }
    }
    channel
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_dbc_encodes_and_decodes_engine_signals_and_enums() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("powertrain.dbc");
        std::fs::write(&path, include_str!("../../../demo/powertrain.dbc")).unwrap();
        let dbc = ParsedDbc::new(path.to_str().unwrap()).unwrap();
        assert_eq!(dbc.messages.len(), 3);
        let msg = &dbc.messages[&256];
        let bytes = msg.encode_signals(&HashMap::from([
            ("EngineRPM".into(), 2500.0),
            ("ThrottlePercent".into(), 65.0),
            ("EngineState".into(), 2.0),
        ]));
        let decoded = msg.decode_data(&bytes);
        assert_eq!(decoded.signals[0].physical, 2500.0);
        assert_eq!(decoded.signals[4].physical, 65.0);
        assert!(msg.signals[3].enum_values.iter().any(|v| v.description == "Running"));
        assert_eq!(dbc.messages[&768].signals.len(), 3);
        let channel = demo_channel(dbc, 1000);
        assert_eq!(channel.frames.len(), 903);
        assert_eq!(channel.frames.front().unwrap().timestamp_ms, 1000);
        assert_eq!(channel.frames.back().unwrap().timestamp_ms, 31000);
        assert!(channel.frames.iter().all(|f| f.message_name.is_some() && !f.signals.is_empty()));
        let rpm = channel.get_signal_history(256, "EngineRPM", 0);
        assert_eq!(rpm.len(), 301);
        assert!(rpm.iter().any(|sample| sample.value > 3500.0));
        assert!(rpm.iter().any(|sample| sample.value < 1500.0));
        assert_eq!(channel.get_frames(1, 1)[0].direction, "tx");
    }
}
