//! Input injection into the host: the interface, and a logger for tests.
//! Linux: `uinput.rs`; Windows: `win/input.rs`.

use moon_proto::control::input::Event;
use tracing::info;

pub trait Injector: Send {
    fn handle(&mut self, ev: &Event);
    /// Release every key/button still held (on disconnect).
    fn reset(&mut self) {}
    /// Rumble changes asked by games on the virtual gamepads since the last call:
    /// (controller, low-frequency / strong motor, high-frequency / weak motor).
    fn rumble(&mut self) -> Vec<(u16, u16, u16)> {
        Vec::new()
    }
}

/// Keyboard layout to type client text with: (XKB layout, variant).
#[cfg(target_os = "linux")]
pub type LayoutFn = Box<dyn Fn() -> (String, String) + Send>;

/// Records events without touching the system (tests, `--no-input`).
#[derive(Default)]
pub struct LogInjector {
    pub count: u64,
}

impl Injector for LogInjector {
    fn handle(&mut self, ev: &Event) {
        self.count += 1;
        info!("input: {ev:?}");
    }
}

