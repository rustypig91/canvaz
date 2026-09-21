use super::StoredFrame;
use serde::Serialize;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub(super) const HEADER: &str = "timestamp_ms,elapsed_s,channel,can_id,direction,dlc,data,message,pgn,src,dst,prio,reassembled\n";

pub(super) fn write_frame(writer: &mut impl Write, channel: &str, f: &StoredFrame, start_ms: u64) -> std::io::Result<()> {
    let elapsed = (f.timestamp_ms as f64 - start_ms as f64) / 1000.0;
    let id = if f.is_extended {
        format!("{:08X}", f.can_id)
    } else {
        format!("{:03X}", f.can_id)
    };
    let data = f.data.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(" ");
    let message = f.message_name.as_deref().or(f.error.as_deref()).unwrap_or("").replace('"', "\"\"");
    let j1939 = f
        .j1939
        .map(|j| format!("{:X},{:02X},{:02X},{}", j.pgn, j.sa, j.da, j.priority))
        .unwrap_or_else(|| ",,,".into());
    writeln!(
        writer,
        "{},{:.3},\"{}\",{},{},{},\"{}\",\"{}\",{},{}",
        f.timestamp_ms,
        elapsed,
        channel.replace('"', "\"\""),
        id,
        f.direction,
        f.data.len(),
        data,
        message,
        j1939,
        f.reassembled as u8
    )
}

#[derive(Clone, Default, Serialize)]
pub struct RecordingStatus {
    pub active: bool,
    pub path: String,
    pub frames: u64,
    pub bytes: u64,
    pub error: Option<String>,
}

#[derive(Default)]
pub(super) struct Recorder {
    sender: Option<mpsc::SyncSender<(String, StoredFrame)>>,
    worker: Option<JoinHandle<()>>,
    status: Arc<Mutex<RecordingStatus>>,
}

impl Recorder {
    pub fn enabled(&self) -> bool {
        self.sender.is_some()
    }

    pub fn status(&self) -> RecordingStatus {
        self.status.lock().unwrap().clone()
    }

    pub fn start(&mut self, path: String, start_ms: u64) -> Result<(), String> {
        if self.status().active {
            return Err("Already recording".into());
        }
        self.stop();
        let mut file = BufWriter::new(File::create(&path).map_err(|e| e.to_string())?);
        file.write_all(HEADER.as_bytes())
            .and_then(|_| file.flush())
            .map_err(|e| e.to_string())?;
        let status = Arc::new(Mutex::new(RecordingStatus {
            active: true,
            path,
            bytes: HEADER.len() as u64,
            ..Default::default()
        }));
        let progress = Arc::clone(&status);
        // Bounded memory; never stall CAN reception waiting for a slow disk.
        let (sender, receiver) = mpsc::sync_channel::<(String, StoredFrame)>(8192);
        let worker = std::thread::Builder::new()
            .name("can-recording".into())
            .spawn(move || {
                let result = (|| -> std::io::Result<()> {
                    loop {
                        let next = receiver.recv_timeout(Duration::from_millis(250));
                        let disconnected = matches!(next, Err(mpsc::RecvTimeoutError::Disconnected));
                        let mut count = 0;
                        if let Ok((channel, frame)) = next {
                            write_frame(&mut file, &channel, &frame, start_ms)?;
                            count += 1;
                            for (channel, frame) in receiver.try_iter().take(1023) {
                                write_frame(&mut file, &channel, &frame, start_ms)?;
                                count += 1;
                            }
                        }
                        file.flush()?;
                        let bytes = file.get_ref().metadata()?.len();
                        let mut state = progress.lock().unwrap();
                        state.frames += count;
                        state.bytes = bytes;
                        if disconnected {
                            break;
                        }
                    }
                    Ok(())
                })();
                let mut state = progress.lock().unwrap();
                state.active = false;
                if let Err(error) = result {
                    state.error = Some(format!("Recording failed: {error}"));
                }
            })
            .map_err(|e| e.to_string())?;
        self.status = status;
        self.sender = Some(sender);
        self.worker = Some(worker);
        Ok(())
    }

    pub fn record(&mut self, channel: &str, frame: &StoredFrame) {
        let Some(sender) = &self.sender else { return };
        let mut copy = frame.clone();
        copy.signals.clear();
        if let Err(error) = sender.try_send((channel.to_owned(), copy)) {
            if matches!(error, mpsc::TrySendError::Full(_)) {
                self.status.lock().unwrap().error =
                    Some("Recording stopped: disk writer could not keep up. The file is incomplete.".into());
            }
            // Disconnect so the writer drains accepted frames and finalizes the file.
            self.sender = None;
        }
    }

    pub fn stop(&mut self) -> RecordingStatus {
        self.sender = None;
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                let mut state = self.status.lock().unwrap();
                state.active = false;
                state.error = Some("Recording writer stopped unexpectedly".into());
            }
        }
        self.status()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(timestamp_ms: u64) -> StoredFrame {
        StoredFrame {
            can_id: 0x123,
            is_extended: false,
            data: vec![0xAB, 0xCD],
            timestamp_ms,
            direction: "rx",
            message_name: Some("Message, \"quoted\"".into()),
            j1939: None,
            reassembled: false,
            error: None,
            dbc_msg_id: None,
            signals: vec![],
        }
    }

    #[test]
    fn stop_drains_frames_beyond_retention_and_escapes_csv() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.csv");
        let mut recorder = Recorder::default();
        recorder.start(path.to_string_lossy().into_owned(), 1000).unwrap();
        for timestamp in [1000, 32000, 64000] {
            recorder.record("CAN, \"one\"", &frame(timestamp));
        }
        let status = recorder.stop();
        assert!(!status.active);
        assert!(status.error.is_none());
        assert_eq!(status.frames, 3);
        let csv = std::fs::read_to_string(path).unwrap();
        assert_eq!(status.bytes, csv.len() as u64);
        assert_eq!(csv.lines().count(), 4);
        assert!(csv.contains("1000,0.000,\"CAN, \"\"one\"\"\",123,rx,2,\"AB CD\",\"Message, \"\"quoted\"\"\""));
        assert!(csv.contains("64000,63.000"));
    }

    #[test]
    fn lifecycle_rejects_double_start_and_allows_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capture.csv").to_string_lossy().into_owned();
        let mut recorder = Recorder::default();
        assert!(recorder.start(dir.path().to_string_lossy().into_owned(), 0).is_err());
        assert!(!recorder.status().active);
        recorder.start(path.clone(), 0).unwrap();
        assert!(recorder.start(path.clone(), 0).is_err());
        recorder.record("CAN", &frame(1000));
        assert_eq!(recorder.stop().frames, 1);
        assert_eq!(recorder.stop().frames, 1);
        recorder.start(path, 0).unwrap();
        assert_eq!(recorder.stop().frames, 0);
    }

    #[test]
    fn full_queue_stops_without_silently_dropping_frames() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let mut recorder = Recorder::default();
        recorder.sender = Some(sender);
        recorder.record("CAN", &frame(1));
        recorder.record("CAN", &frame(2));
        assert!(!recorder.enabled());
        assert!(recorder.status().error.unwrap().contains("incomplete"));
    }
}
