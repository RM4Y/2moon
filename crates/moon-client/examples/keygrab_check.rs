//! Opens a small window for a second and asks the compositor to hand it the
//! system shortcuts (what the stream window does); prints the answer.
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

#[derive(Default)]
struct App {
    window: Option<Window>,
    grab: Option<moon_client::keygrab::KeyGrab>,
    start: Option<std::time::Instant>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let w = el.create_window(Window::default_attributes().with_title("2moon keygrab check")).unwrap();
        let mut g = moon_client::keygrab::KeyGrab::new(w.display_handle().unwrap().as_raw(), w.window_handle().unwrap().as_raw()).expect("no shortcut capture on this platform");
        g.set(true, None);
        self.grab = Some(g);
        self.window = Some(w);
        self.start = Some(std::time::Instant::now());
        el.set_control_flow(ControlFlow::Poll);
    }
    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, ev: WindowEvent) {
        if let WindowEvent::KeyboardInput { event, .. } = &ev {
            println!("key {:?} {:?}", event.physical_key, event.state);
        }
        if let WindowEvent::Focused(f) = &ev {
            println!("focused: {f}");
        }
        if matches!(ev, WindowEvent::CloseRequested) {
            el.exit();
        }
    }
    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.start.is_some_and(|t| t.elapsed() > std::time::Duration::from_millis(std::env::var("KEYGRAB_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500))) {
            println!("compositor answer: {:?}", self.grab.as_mut().unwrap().granted());
            el.exit();
        }
    }
}

fn main() {
    let el = EventLoop::new().unwrap();
    el.run_app(&mut App::default()).unwrap();
}
