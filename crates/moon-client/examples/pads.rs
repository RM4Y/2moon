//! List the game controllers SDL sees, as the client will forward them, and
//! print their state for a few seconds: `cargo run --example pads [seconds]`.
use moon_client::sdl;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let api = sdl::api()?;
    api.hint("SDL_NO_SIGNAL_HANDLERS", "1");
    api.hint("SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS", "1");
    anyhow::ensure!(unsafe { (api.init_subsystem)(sdl::INIT_GAMEPAD) }, "SDL init: {}", api.error());
    println!("SDL {}", api.version());
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut ev = sdl::Event::new();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(secs.max(1));
    let mut pads = vec![];
    while std::time::Instant::now() < end {
        while unsafe { (api.wait_event_timeout)(&mut ev, 50) } {
            if ev.kind() == sdl::EVENT_GAMEPAD_ADDED {
                let p = unsafe { (api.open_gamepad)(ev.which()) };
                let name = unsafe { sdl::cstr((api.gamepad_name)(p)) };
                let kind = unsafe { sdl::cstr((api.type_string)((api.gamepad_type)(p))) };
                let path = unsafe { sdl::cstr((api.gamepad_path)(p)) };
                println!("pad {}: {name} {:04x}:{:04x} type={kind} {path}", ev.which(), unsafe { (api.gamepad_vendor)(p) }, unsafe { (api.gamepad_product)(p) });
                pads.push(p);
            } else if secs > 0 && ev.kind() == sdl::EVENT_GAMEPAD_BUTTON_DOWN {
                println!("button {} on {}", ev.0[20], ev.which());
            }
        }
    }
    if pads.is_empty() {
        println!("no controller");
    }
    Ok(())
}
