//! Audio playback/capture through the PulseAudio "simple" API (also served
//! by PipeWire), loaded at runtime from `libpulse-simple.so.0`.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Result};
use libloading::Library;

const PA_STREAM_PLAYBACK: c_int = 1;
const PA_STREAM_RECORD: c_int = 2;
const PA_SAMPLE_S16LE: c_int = 3;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

#[repr(C)]
struct ChannelMap {
    channels: u8,
    map: [c_int; 32],
}

/// GameStream channel order: FL FR FC LFE BL BR SL SR (PulseAudio positions).
fn channel_map(channels: u8) -> Option<ChannelMap> {
    const ORDER: [c_int; 8] = [1, 2, 3, 7, 5, 6, 10, 11];
    if channels <= 2 {
        return None;
    }
    let mut m = ChannelMap { channels, map: [0; 32] };
    m.map[..channels as usize].copy_from_slice(&ORDER[..channels as usize]);
    Some(m)
}

type New = unsafe extern "C" fn(*const c_char, *const c_char, c_int, *const c_char, *const c_char, *const SampleSpec, *const ChannelMap, *const BufferAttr, *mut c_int) -> *mut c_void;
type Io = unsafe extern "C" fn(*mut c_void, *const c_void, usize, *mut c_int) -> c_int;
type Read = unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_int) -> c_int;
type Free = unsafe extern "C" fn(*mut c_void);
type Latency = unsafe extern "C" fn(*mut c_void, *mut c_int) -> u64;
type Flush = unsafe extern "C" fn(*mut c_void, *mut c_int) -> c_int;
type Strerror = unsafe extern "C" fn(c_int) -> *const c_char;

struct Api {
    _simple: Library,
    _pulse: Library,
    new: New,
    write: Io,
    read: Read,
    free: Free,
    latency: Latency,
    flush: Flush,
    strerror: Strerror,
}

fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let simple = Library::new("libpulse-simple.so.0").map_err(|e| format!("cannot load libpulse-simple: {e}"))?;
        let pulse = Library::new("libpulse.so.0").map_err(|e| format!("cannot load libpulse: {e}"))?;
        macro_rules! sym {
            ($l:expr, $n:literal) => {
                *$l.get($n).map_err(|e| format!("pulse symbol {}: {e}", String::from_utf8_lossy($n)))?
            };
        }
        Ok(Api {
            new: sym!(simple, b"pa_simple_new\0"),
            write: sym!(simple, b"pa_simple_write\0"),
            read: sym!(simple, b"pa_simple_read\0"),
            free: sym!(simple, b"pa_simple_free\0"),
            latency: sym!(simple, b"pa_simple_get_latency\0"),
            flush: sym!(simple, b"pa_simple_flush\0"),
            strerror: sym!(pulse, b"pa_strerror\0"),
            _simple: simple,
            _pulse: pulse,
        })
    })
    .as_ref()
    .map_err(|e| anyhow!("{e}"))
}

fn err_str(a: &Api, e: c_int) -> String {
    unsafe { CStr::from_ptr((a.strerror)(e)).to_string_lossy().into_owned() }
}

pub struct PulseStream {
    s: *mut c_void,
    pub channels: u8,
}

unsafe impl Send for PulseStream {}

impl PulseStream {
    fn open(dir: c_int, device: Option<&str>, name: &str, rate: u32, channels: u8, latency_bytes: u32) -> Result<Self> {
        let a = api()?;
        let app = CString::new("2moon")?;
        let sname = CString::new(name)?;
        let dev = device.map(CString::new).transpose()?;
        let spec = SampleSpec { format: PA_SAMPLE_S16LE, rate, channels };
        let attr = if dir == PA_STREAM_PLAYBACK {
            BufferAttr { maxlength: u32::MAX, tlength: latency_bytes, prebuf: u32::MAX, minreq: u32::MAX, fragsize: u32::MAX }
        } else {
            BufferAttr { maxlength: u32::MAX, tlength: u32::MAX, prebuf: u32::MAX, minreq: u32::MAX, fragsize: latency_bytes }
        };
        let mut err = 0;
        let map = channel_map(channels);
        let s = unsafe {
            (a.new)(std::ptr::null(), app.as_ptr(), dir, dev.as_ref().map(|d| d.as_ptr()).unwrap_or(std::ptr::null()), sname.as_ptr(), &spec, map.as_ref().map(|m| m as *const _).unwrap_or(std::ptr::null()), &attr, &mut err)
        };
        if s.is_null() {
            bail!("pa_simple_new: {}", err_str(a, err));
        }
        Ok(Self { s, channels })
    }

    /// Playback stream with a target buffer of `latency_ms`.
    pub fn playback(rate: u32, channels: u8, latency_ms: u32) -> Result<Self> {
        Self::open(PA_STREAM_PLAYBACK, None, "2moon stream", rate, channels, rate * channels as u32 * 2 * latency_ms / 1000)
    }

    /// Record stream. `device` = None records the default source; use
    /// `"@DEFAULT_MONITOR@"` to capture what the computer is playing.
    pub fn record(device: Option<&str>, rate: u32, channels: u8, fragment_ms: u32) -> Result<Self> {
        Self::open(PA_STREAM_RECORD, device, "2moon capture", rate, channels, rate * channels as u32 * 2 * fragment_ms / 1000)
    }

    pub fn write(&mut self, pcm: &[i16]) -> Result<()> {
        let a = api()?;
        let mut err = 0;
        if unsafe { (a.write)(self.s, pcm.as_ptr() as *const c_void, std::mem::size_of_val(pcm), &mut err) } < 0 {
            bail!("pa_simple_write: {}", err_str(a, err));
        }
        Ok(())
    }

    /// Blocking read filling `pcm` completely.
    pub fn read(&mut self, pcm: &mut [i16]) -> Result<()> {
        let a = api()?;
        let mut err = 0;
        if unsafe { (a.read)(self.s, pcm.as_mut_ptr() as *mut c_void, std::mem::size_of_val(pcm), &mut err) } < 0 {
            bail!("pa_simple_read: {}", err_str(a, err));
        }
        Ok(())
    }

    pub fn latency_us(&self) -> u64 {
        let mut err = 0;
        api().map(|a| unsafe { (a.latency)(self.s, &mut err) }).unwrap_or(0)
    }

    pub fn flush(&mut self) {
        let mut err = 0;
        if let Ok(a) = api() {
            unsafe { (a.flush)(self.s, &mut err) };
        }
    }
}

impl Drop for PulseStream {
    fn drop(&mut self) {
        if let Ok(a) = api() {
            unsafe { (a.free)(self.s) }
        }
    }
}
