// enum Channel {
//     #[cfg(feature = "linux-can")]
//     SocketCan(SocketCanChannel),
//     #[cfg(feature = "kvaser")]
//     Kvaser(KvaserBackendChannel),
// }

mod example_backend;

use example_backend::ExampleBackend;

enum CanBackend {
    Example(ExampleBackend),
}

impl CanBackend {
    fn open(&mut self, channel: u8, bitrate: u32) -> Result<(), String> {
        match self {
            CanBackend::Example(backend) => backend.open(channel, bitrate),
        }
    }
}

struct Can {

}

// class Can

// fn list_can_channels()
