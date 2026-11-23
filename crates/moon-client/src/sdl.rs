//! SDL3 loaded at run time, for game controllers (HIDAPI drivers + mapping database).
//! Search: `MOON_SDL`, the bundled copy next to the executable, then the system's.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use libloading::Library;

pub type JoystickId = u32;
#[repr(C)]
pub struct Gamepad {
    _p: [u8; 0],
}

pub const INIT_GAMEPAD: u32 = 0x0000_2000;

pub const EVENT_GAMEPAD_AXIS_MOTION: u32 = 0x650;
pub const EVENT_GAMEPAD_BUTTON_DOWN: u32 = 0x651;
pub const EVENT_GAMEPAD_BUTTON_UP: u32 = 0x652;
pub const EVENT_GAMEPAD_ADDED: u32 = 0x653;
pub const EVENT_GAMEPAD_REMOVED: u32 = 0x654;
pub const EVENT_GAMEPAD_REMAPPED: u32 = 0x655;

/// SDL_GamepadButton (positional: South is the bottom face button on every controller).
pub mod button {
    pub const SOUTH: i32 = 0;
    pub const EAST: i32 = 1;
    pub const WEST: i32 = 2;
    pub const NORTH: i32 = 3;
    pub const BACK: i32 = 4;
    pub const GUIDE: i32 = 5;
    pub const START: i32 = 6;
    pub const LEFT_STICK: i32 = 7;
    pub const RIGHT_STICK: i32 = 8;
    pub const LEFT_SHOULDER: i32 = 9;
    pub const RIGHT_SHOULDER: i32 = 10;
    pub const DPAD_UP: i32 = 11;
    pub const DPAD_DOWN: i32 = 12;
    pub const DPAD_LEFT: i32 = 13;
    pub const DPAD_RIGHT: i32 = 14;
}

/// SDL_GamepadAxis. Sticks are -32768..32767 with Y pointing down; triggers 0..32767.
pub mod axis {
    pub const LEFTX: i32 = 0;
    pub const LEFTY: i32 = 1;
    pub const RIGHTX: i32 = 2;
    pub const RIGHTY: i32 = 3;
    pub const LEFT_TRIGGER: i32 = 4;
    pub const RIGHT_TRIGGER: i32 = 5;
}

/// SDL_Event: a 128-byte union; the type is the first u32, a gamepad device
/// event's joystick id is at offset 16.
#[repr(C, align(8))]
pub struct Event(pub [u8; 128]);

impl Event {
    pub fn new() -> Self {
        Event([0; 128])
    }
    pub fn kind(&self) -> u32 {
        u32::from_ne_bytes(self.0[0..4].try_into().unwrap())
    }
    pub fn which(&self) -> JoystickId {
        u32::from_ne_bytes(self.0[16..20].try_into().unwrap())
    }
}

pub struct Api {
    _lib: Library,
    pub set_hint: unsafe extern "C" fn(*const c_char, *const c_char) -> bool,
    pub init_subsystem: unsafe extern "C" fn(u32) -> bool,
    pub quit_subsystem: unsafe extern "C" fn(u32),
    pub get_error: unsafe extern "C" fn() -> *const c_char,
    pub get_version: unsafe extern "C" fn() -> c_int,
    pub free: unsafe extern "C" fn(*mut c_void),
    pub wait_event_timeout: unsafe extern "C" fn(*mut Event, i32) -> bool,
    pub poll_event: unsafe extern "C" fn(*mut Event) -> bool,
    pub get_gamepads: unsafe extern "C" fn(*mut c_int) -> *mut JoystickId,
    pub open_gamepad: unsafe extern "C" fn(JoystickId) -> *mut Gamepad,
    pub close_gamepad: unsafe extern "C" fn(*mut Gamepad),
    pub gamepad_name: unsafe extern "C" fn(*mut Gamepad) -> *const c_char,
    pub gamepad_vendor: unsafe extern "C" fn(*mut Gamepad) -> u16,
    pub gamepad_product: unsafe extern "C" fn(*mut Gamepad) -> u16,
    pub gamepad_type: unsafe extern "C" fn(*mut Gamepad) -> c_int,
    pub type_string: unsafe extern "C" fn(c_int) -> *const c_char,
    pub gamepad_path: unsafe extern "C" fn(*mut Gamepad) -> *const c_char,
    pub gamepad_button: unsafe extern "C" fn(*mut Gamepad, c_int) -> bool,
    pub gamepad_axis: unsafe extern "C" fn(*mut Gamepad, c_int) -> i16,
    pub rumble_gamepad: unsafe extern "C" fn(*mut Gamepad, u16, u16, u32) -> bool,
    pub rumble_triggers: unsafe extern "C" fn(*mut Gamepad, u16, u16, u32) -> bool,
}

fn candidates() -> Vec<std::path::PathBuf> {
    let mut v = vec![];
    if let Some(p) = std::env::var_os("MOON_SDL") {
        v.push(p.into());
    }
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())) {
        if cfg!(windows) {
            v.push(dir.join("SDL3.dll"));
        } else if cfg!(target_os = "macos") {
            v.push(dir.join("lib").join("libSDL3.0.dylib"));
        } else {
            v.push(dir.join("lib").join("libSDL3.so.0"));
        }
    }
    if cfg!(target_os = "macos") {
        // Homebrew's (`brew install sdl3`), then the system search path.
        for p in ["/opt/homebrew/lib/libSDL3.0.dylib", "/usr/local/lib/libSDL3.0.dylib"] {
            v.push(p.into());
        }
        v.push("libSDL3.0.dylib".into());
        return v;
    }
    v.push(if cfg!(windows) { "SDL3.dll" } else { "libSDL3.so.0" }.into());
    v
}

/// The loaded library, or why it could not be loaded.
pub fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let names = candidates();
        let mut errors = vec![];
        let lib = names
            .iter()
            .filter(|n| n.components().count() == 1 || n.exists())
            .find_map(|n| match Library::new(n) {
                Ok(l) => {
                    tracing::info!("game controllers: using SDL3 from {}", n.display());
                    Some(l)
                }
                Err(e) => {
                    errors.push(format!("{}: {e}", n.display()));
                    None
                }
            })
            .ok_or_else(|| format!("cannot load SDL3 ({})", errors.join("; ")))?;
        macro_rules! sym {
            ($n:literal) => {
                *lib.get(concat!($n, "\0").as_bytes()).map_err(|e| format!("SDL3 symbol {}: {e}", $n))?
            };
        }
        Ok(Api {
            set_hint: sym!("SDL_SetHint"),
            init_subsystem: sym!("SDL_InitSubSystem"),
            quit_subsystem: sym!("SDL_QuitSubSystem"),
            get_error: sym!("SDL_GetError"),
            get_version: sym!("SDL_GetVersion"),
            free: sym!("SDL_free"),
            wait_event_timeout: sym!("SDL_WaitEventTimeout"),
            poll_event: sym!("SDL_PollEvent"),
            get_gamepads: sym!("SDL_GetGamepads"),
            open_gamepad: sym!("SDL_OpenGamepad"),
            close_gamepad: sym!("SDL_CloseGamepad"),
            gamepad_name: sym!("SDL_GetGamepadName"),
            gamepad_vendor: sym!("SDL_GetGamepadVendor"),
            gamepad_product: sym!("SDL_GetGamepadProduct"),
            gamepad_type: sym!("SDL_GetGamepadType"),
            type_string: sym!("SDL_GetGamepadStringForType"),
            gamepad_path: sym!("SDL_GetGamepadPath"),
            gamepad_button: sym!("SDL_GetGamepadButton"),
            gamepad_axis: sym!("SDL_GetGamepadAxis"),
            rumble_gamepad: sym!("SDL_RumbleGamepad"),
            rumble_triggers: sym!("SDL_RumbleGamepadTriggers"),
            _lib: lib,
        })
    })
    .as_ref()
    .map_err(|e| anyhow!("{e}"))
}

impl Api {
    pub fn error(&self) -> String {
        unsafe { cstr((self.get_error)()) }
    }
    pub fn hint(&self, name: &str, value: &str) {
        let (n, v) = (CString::new(name).unwrap(), CString::new(value).unwrap());
        unsafe { (self.set_hint)(n.as_ptr(), v.as_ptr()) };
    }
    /// "3.4.10"
    pub fn version(&self) -> String {
        let v = unsafe { (self.get_version)() };
        format!("{}.{}.{}", v / 1_000_000, v / 1000 % 1000, v % 1000)
    }
    /// Gamepads connected now.
    pub fn gamepads(&self) -> Vec<JoystickId> {
        unsafe {
            let mut n = 0;
            let p = (self.get_gamepads)(&mut n);
            if p.is_null() {
                return vec![];
            }
            let v = std::slice::from_raw_parts(p, n.max(0) as usize).to_vec();
            (self.free)(p.cast());
            v
        }
    }
}

/// Copy a C string owned by SDL (empty when null).
pub unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}
