//! SAE J1939 protocol support: 29-bit ID decoding (PGN/SA/DA/priority) and
//! transport-protocol reassembly (TP.CM / TP.DT, both BAM broadcasts and
//! RTS/CTS connections observed passively on the bus).

use std::collections::HashMap;

use serde::Serialize;

use crate::can_communication::CanFrame;

/// PGN of the transport-protocol connection-management message (TP.CM).
pub const PGN_TP_CM: u32 = 0x00EC00;
/// PGN of the transport-protocol data-transfer message (TP.DT).
pub const PGN_TP_DT: u32 = 0x00EB00;

/// Largest message the transport protocol can carry: 255 packets × 7 bytes.
const TP_MAX_SIZE: usize = 1785;
/// A session with no traffic for this long is abandoned (spec T1 = 750 ms
/// between consecutive TP.DT packets; use a slightly generous bound).
const TP_SESSION_TIMEOUT_MS: u64 = 1250;

const TP_CM_RTS: u8 = 16;
const TP_CM_BAM: u8 = 32;
const TP_CM_ABORT: u8 = 255;

/// J1939 view of a 29-bit CAN identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct J1939Info {
    /// 18-bit parameter group number (EDP/DP included).
    pub pgn: u32,
    pub priority: u8,
    /// Source address.
    pub sa: u8,
    /// Destination address; 0xFF for PDU2 (broadcast) groups.
    pub da: u8,
}

/// Split a 29-bit identifier into priority / PGN / SA / DA.
pub fn decode_id(can_id: u32) -> J1939Info {
    let priority = ((can_id >> 26) & 0x7) as u8;
    let pf = ((can_id >> 16) & 0xFF) as u8;
    let ps = ((can_id >> 8) & 0xFF) as u8;
    let sa = (can_id & 0xFF) as u8;
    // EDP+DP+PF form the upper PGN bits; PS belongs to the PGN only for PDU2.
    let mut pgn = (can_id >> 8) & 0x3_FF00;
    let da = if pf < 240 {
        ps
    } else {
        pgn |= ps as u32;
        0xFF
    };
    J1939Info { pgn, priority, sa, da }
}

/// Compose a 29-bit identifier from its J1939 parts. For PDU1 groups the
/// destination address is placed in the PS field; for PDU2 the PGN's own low
/// byte is used and `da` is ignored.
pub fn build_id(priority: u8, pgn: u32, da: u8, sa: u8) -> u32 {
    let pf = (pgn >> 8) & 0xFF;
    let ps = if pf < 240 { da as u32 } else { pgn & 0xFF };
    ((priority as u32 & 0x7) << 26) | ((pgn & 0x3_FF00) << 8) | (ps << 8) | sa as u32
}

struct TpSession {
    /// PGN of the message being transferred (announced in TP.CM byte 5-7).
    pgn: u32,
    priority: u8,
    da: u8,
    total_size: usize,
    num_packets: u8,
    next_seq: u8,
    data: Vec<u8>,
    last_ts: u64,
}

/// Reassembles multi-packet transport-protocol transfers, one session per
/// (source, destination) pair as the spec allows.
#[derive(Default)]
pub struct TpReassembler {
    sessions: HashMap<(u8, u8), TpSession>,
}

impl TpReassembler {
    /// Feed one TP.CM / TP.DT frame. Returns the fully reassembled message as a
    /// synthetic CAN frame (announced PGN rebuilt into the ID, data longer than
    /// 8 bytes) once a transfer completes. Non-TP frames must not be passed in.
    pub fn handle_frame(&mut self, frame: &CanFrame, ts: u64) -> Option<CanFrame> {
        self.drop_stale(ts);

        let id = decode_id(frame.can_id);
        match id.pgn {
            PGN_TP_CM => {
                self.handle_cm(id, &frame.data, ts);
                None
            }
            PGN_TP_DT => self.handle_dt(id, &frame.data, ts),
            _ => None,
        }
    }

    fn handle_cm(&mut self, id: J1939Info, data: &[u8], ts: u64) {
        if data.len() < 8 {
            return;
        }
        let key = (id.sa, id.da);
        match data[0] {
            TP_CM_RTS | TP_CM_BAM => {
                let total_size = u16::from_le_bytes([data[1], data[2]]) as usize;
                let num_packets = data[3];
                let pgn = u32::from(data[5]) | u32::from(data[6]) << 8 | u32::from(data[7]) << 16;
                if total_size == 0 || total_size > TP_MAX_SIZE || num_packets == 0 {
                    self.sessions.remove(&key);
                    return;
                }
                // A new announcement replaces any in-flight session for the pair.
                self.sessions.insert(
                    key,
                    TpSession {
                        pgn,
                        priority: id.priority,
                        da: id.da,
                        total_size,
                        num_packets,
                        next_seq: 1,
                        data: Vec::with_capacity(total_size),
                        last_ts: ts,
                    },
                );
            }
            TP_CM_ABORT => {
                self.sessions.remove(&key);
            }
            // CTS / EndOfMsgAck flow from the receiver; nothing to reassemble.
            _ => {}
        }
    }

    fn handle_dt(&mut self, id: J1939Info, data: &[u8], ts: u64) -> Option<CanFrame> {
        if data.len() < 2 {
            return None;
        }
        let key = (id.sa, id.da);
        let session = self.sessions.get_mut(&key)?;

        let seq = data[0];
        if seq != session.next_seq {
            // Out-of-order packet: the transfer is broken, discard the session.
            self.sessions.remove(&key);
            return None;
        }
        session.next_seq = session.next_seq.wrapping_add(1);
        session.last_ts = ts;
        session.data.extend_from_slice(&data[1..]);

        if seq < session.num_packets {
            return None;
        }

        let mut session = self.sessions.remove(&key).expect("session present");
        if session.data.len() < session.total_size {
            return None; // announced more data than the packets carried
        }
        session.data.truncate(session.total_size);
        Some(CanFrame {
            can_id: build_id(session.priority, session.pgn, session.da, id.sa),
            is_extended: true,
            data: session.data,
            timestamp_ms: Some(ts),
        })
    }

    fn drop_stale(&mut self, now: u64) {
        self.sessions.retain(|_, s| now.saturating_sub(s.last_ts) < TP_SESSION_TIMEOUT_MS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(can_id: u32, data: Vec<u8>) -> CanFrame {
        CanFrame {
            can_id,
            is_extended: true,
            data,
            timestamp_ms: None,
        }
    }

    #[test]
    fn decode_pdu2_broadcast() {
        // 0x18FEF117: prio 6, PGN 0xFEF1 (CCVS), SA 0x17
        let id = decode_id(0x18FEF117);
        assert_eq!(id.priority, 6);
        assert_eq!(id.pgn, 0xFEF1);
        assert_eq!(id.sa, 0x17);
        assert_eq!(id.da, 0xFF);
    }

    #[test]
    fn decode_pdu1_destination_specific() {
        // 0x0CEF2A03: prio 3, PGN 0xEF00, DA 0x2A, SA 0x03
        let id = decode_id(0x0CEF2A03);
        assert_eq!(id.priority, 3);
        assert_eq!(id.pgn, 0xEF00);
        assert_eq!(id.da, 0x2A);
        assert_eq!(id.sa, 0x03);
    }

    #[test]
    fn build_roundtrip() {
        assert_eq!(build_id(6, 0xFEF1, 0xFF, 0x17), 0x18FEF117);
        assert_eq!(build_id(3, 0xEF00, 0x2A, 0x03), 0x0CEF2A03);
    }

    #[test]
    fn bam_reassembly() {
        let mut tp = TpReassembler::default();
        // BAM from SA 0x21: PGN 0xFECA (DM1), 12 bytes in 2 packets.
        let cm_id = build_id(7, PGN_TP_CM, 0xFF, 0x21);
        assert!(tp
            .handle_frame(&frame(cm_id, vec![32, 12, 0, 2, 0xFF, 0xCA, 0xFE, 0x00]), 0)
            .is_none());

        let dt_id = build_id(7, PGN_TP_DT, 0xFF, 0x21);
        assert!(tp.handle_frame(&frame(dt_id, vec![1, 1, 2, 3, 4, 5, 6, 7]), 10).is_none());
        let done = tp.handle_frame(&frame(dt_id, vec![2, 8, 9, 10, 11, 12, 0xFF, 0xFF]), 20).unwrap();

        assert_eq!(done.can_id, build_id(7, 0xFECA, 0xFF, 0x21));
        assert_eq!(done.data, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(done.timestamp_ms, Some(20));
    }

    #[test]
    fn rts_reassembly_destination_specific() {
        let mut tp = TpReassembler::default();
        // RTS SA 0x10 → DA 0x20: PGN 0xEF00 (prop A), 9 bytes in 2 packets.
        let cm_id = build_id(6, PGN_TP_CM, 0x20, 0x10);
        tp.handle_frame(&frame(cm_id, vec![16, 9, 0, 2, 0xFF, 0x00, 0xEF, 0x00]), 0);

        let dt_id = build_id(6, PGN_TP_DT, 0x20, 0x10);
        assert!(tp.handle_frame(&frame(dt_id, vec![1, 1, 2, 3, 4, 5, 6, 7]), 1).is_none());
        let done = tp
            .handle_frame(&frame(dt_id, vec![2, 8, 9, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]), 2)
            .unwrap();

        assert_eq!(done.can_id, build_id(6, 0xEF00, 0x20, 0x10));
        assert_eq!(done.data.len(), 9);
        assert_eq!(done.data, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn out_of_order_drops_session() {
        let mut tp = TpReassembler::default();
        let cm_id = build_id(7, PGN_TP_CM, 0xFF, 0x21);
        tp.handle_frame(&frame(cm_id, vec![32, 14, 0, 2, 0xFF, 0xCA, 0xFE, 0x00]), 0);
        let dt_id = build_id(7, PGN_TP_DT, 0xFF, 0x21);
        // Sequence 2 first — broken transfer.
        assert!(tp.handle_frame(&frame(dt_id, vec![2, 8, 9, 10, 11, 12, 13, 14]), 1).is_none());
        assert!(tp.handle_frame(&frame(dt_id, vec![1, 1, 2, 3, 4, 5, 6, 7]), 2).is_none());
    }

    #[test]
    fn stale_session_times_out() {
        let mut tp = TpReassembler::default();
        let cm_id = build_id(7, PGN_TP_CM, 0xFF, 0x21);
        tp.handle_frame(&frame(cm_id, vec![32, 14, 0, 2, 0xFF, 0xCA, 0xFE, 0x00]), 0);
        let dt_id = build_id(7, PGN_TP_DT, 0xFF, 0x21);
        // First packet arrives long after the announcement.
        assert!(tp.handle_frame(&frame(dt_id, vec![1, 1, 2, 3, 4, 5, 6, 7]), 5000).is_none());
        assert!(tp.handle_frame(&frame(dt_id, vec![2, 8, 9, 10, 11, 12, 13, 14]), 5010).is_none());
    }

    #[test]
    fn abort_cancels_session() {
        let mut tp = TpReassembler::default();
        let cm_id = build_id(6, PGN_TP_CM, 0x20, 0x10);
        tp.handle_frame(&frame(cm_id, vec![16, 9, 0, 2, 0xFF, 0x00, 0xEF, 0x00]), 0);
        tp.handle_frame(&frame(cm_id, vec![255, 1, 0xFF, 0xFF, 0xFF, 0x00, 0xEF, 0x00]), 1);
        let dt_id = build_id(6, PGN_TP_DT, 0x20, 0x10);
        assert!(tp.handle_frame(&frame(dt_id, vec![1, 1, 2, 3, 4, 5, 6, 7]), 2).is_none());
        assert!(tp
            .handle_frame(&frame(dt_id, vec![2, 8, 9, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]), 3)
            .is_none());
    }
}
