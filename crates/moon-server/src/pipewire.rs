//! Minimal PipeWire video consumer, loaded at runtime from
//! `libpipewire-0.3.so.0` (no headers or bindgen needed).
//!
//! Only the small, ABI-stable part of the API is used: thread loop, context,
//! stream and buffer dequeueing. Format negotiation pods are built by hand.

use std::ffi::{c_char, c_int, c_void, CString};
use std::os::fd::{IntoRawFd, OwnedFd};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use libloading::Library;
use tracing::{debug, info, warn};

// ---- SPA constants -------------------------------------------------------
const SPA_TYPE_ID: u32 = 3;
const SPA_TYPE_INT: u32 = 4;
const SPA_TYPE_RECTANGLE: u32 = 10;
const SPA_TYPE_FRACTION: u32 = 11;
const SPA_TYPE_OBJECT: u32 = 15;
const SPA_TYPE_CHOICE: u32 = 19;
const SPA_TYPE_OBJECT_FORMAT: u32 = 0x40003;
const SPA_TYPE_OBJECT_PARAM_BUFFERS: u32 = 0x40004;
const SPA_PARAM_ENUM_FORMAT: u32 = 3;
const SPA_PARAM_FORMAT: u32 = 4;
const SPA_PARAM_BUFFERS: u32 = 5;
const SPA_FORMAT_MEDIA_TYPE: u32 = 1;
const SPA_FORMAT_MEDIA_SUBTYPE: u32 = 2;
const SPA_FORMAT_VIDEO_FORMAT: u32 = 0x20001;
const SPA_FORMAT_VIDEO_SIZE: u32 = 0x20003;
const SPA_FORMAT_VIDEO_FRAMERATE: u32 = 0x20004;
const SPA_MEDIA_TYPE_VIDEO: u32 = 2;
const SPA_MEDIA_SUBTYPE_RAW: u32 = 1;
const SPA_CHOICE_NONE: u32 = 0;
const SPA_CHOICE_RANGE: u32 = 1;
const SPA_CHOICE_ENUM: u32 = 3;
const SPA_CHOICE_FLAGS: u32 = 4;
const SPA_PARAM_BUFFERS_DATATYPE: u32 = 6;
const SPA_DATA_MEMPTR: u32 = 1;
const SPA_DATA_MEMFD: u32 = 2;
const SPA_DATA_DMABUF: u32 = 3;
const SPA_CHUNK_FLAG_CORRUPTED: i32 = 1;

pub const SPA_VIDEO_FORMAT_RGBX: u32 = 7;
pub const SPA_VIDEO_FORMAT_BGRX: u32 = 8;
pub const SPA_VIDEO_FORMAT_RGBA: u32 = 11;
pub const SPA_VIDEO_FORMAT_BGRA: u32 = 12;

const PW_DIRECTION_INPUT: u32 = 0;
const PW_STREAM_FLAG_AUTOCONNECT: u32 = 1 << 0;
const PW_STREAM_FLAG_MAP_BUFFERS: u32 = 1 << 2;
const PW_ID_ANY: u32 = 0xffff_ffff;

// ---- SPA pod builder / parser ---------------------------------------------

/// Builds SPA pods (8-byte aligned, native endian).
#[derive(Default)]
pub struct Pod(Vec<u8>);

impl Pod {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_ne_bytes());
    }
    fn pad(&mut self) {
        while self.0.len() % 8 != 0 {
            self.0.push(0);
        }
    }
    fn header_at(&mut self, start: usize) {
        let size = (self.0.len() - start - 8) as u32;
        self.0[start..start + 4].copy_from_slice(&size.to_ne_bytes());
    }
    fn begin(&mut self, ty: u32) -> usize {
        let s = self.0.len();
        self.u32(0);
        self.u32(ty);
        s
    }
    fn id(&mut self, v: u32) {
        self.u32(4);
        self.u32(SPA_TYPE_ID);
        self.u32(v);
        self.pad();
    }
    /// A choice of `child_ty` values, each `child_size` bytes long.
    fn choice(&mut self, kind: u32, child_size: u32, child_ty: u32, values: &[u32]) {
        let s = self.begin(SPA_TYPE_CHOICE);
        self.u32(kind);
        self.u32(0);
        self.u32(child_size);
        self.u32(child_ty);
        for v in values {
            self.u32(*v);
        }
        self.header_at(s);
        self.pad();
    }
    fn prop(&mut self, key: u32) {
        self.u32(key);
        self.u32(0);
    }

    /// EnumFormat: raw video in common 32-bit RGB layouts, any size, up to 240 fps.
    pub fn enum_format(fps: u32) -> Self {
        let mut p = Pod::default();
        let s = p.begin(SPA_TYPE_OBJECT);
        p.u32(SPA_TYPE_OBJECT_FORMAT);
        p.u32(SPA_PARAM_ENUM_FORMAT);
        p.prop(SPA_FORMAT_MEDIA_TYPE);
        p.id(SPA_MEDIA_TYPE_VIDEO);
        p.prop(SPA_FORMAT_MEDIA_SUBTYPE);
        p.id(SPA_MEDIA_SUBTYPE_RAW);
        p.prop(SPA_FORMAT_VIDEO_FORMAT);
        p.choice(SPA_CHOICE_ENUM, 4, SPA_TYPE_ID, &[SPA_VIDEO_FORMAT_BGRX, SPA_VIDEO_FORMAT_BGRX, SPA_VIDEO_FORMAT_BGRA, SPA_VIDEO_FORMAT_RGBX, SPA_VIDEO_FORMAT_RGBA]);
        p.prop(SPA_FORMAT_VIDEO_SIZE);
        p.choice(SPA_CHOICE_RANGE, 8, SPA_TYPE_RECTANGLE, &[1920, 1080, 1, 1, 8192, 8192]);
        p.prop(SPA_FORMAT_VIDEO_FRAMERATE);
        p.choice(SPA_CHOICE_RANGE, 8, SPA_TYPE_FRACTION, &[fps, 1, 0, 1, 240, 1]);
        p.header_at(s);
        p
    }

    /// Buffers param restricting buffers to CPU-mappable memory.
    pub fn shm_buffers() -> Self {
        let mut p = Pod::default();
        let s = p.begin(SPA_TYPE_OBJECT);
        p.u32(SPA_TYPE_OBJECT_PARAM_BUFFERS);
        p.u32(SPA_PARAM_BUFFERS);
        p.prop(SPA_PARAM_BUFFERS_DATATYPE);
        p.choice(SPA_CHOICE_FLAGS, 4, SPA_TYPE_INT, &[(1 << SPA_DATA_MEMPTR) | (1 << SPA_DATA_MEMFD)]);
        p.header_at(s);
        p
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoFormat {
    pub format: u32,
    pub width: u32,
    pub height: u32,
}

/// Parse a negotiated `Format` object pod.
pub fn parse_format(pod: &[u8]) -> Option<VideoFormat> {
    let rd = |o: usize| pod.get(o..o + 4).map(|b| u32::from_ne_bytes(b.try_into().unwrap()));
    let size = rd(0)? as usize;
    if rd(4)? != SPA_TYPE_OBJECT || pod.len() < 8 + size {
        return None;
    }
    let end = 8 + size;
    let mut o = 16; // skip object type + id
    let mut f = VideoFormat::default();
    while o + 16 <= end {
        let key = rd(o)?;
        let vsize = rd(o + 8)? as usize;
        let vtype = rd(o + 12)?;
        let body = o + 16;
        // Unwrap single-value choices.
        let (vbody, vty) = if vtype == SPA_TYPE_CHOICE { (body + 16, rd(body + 12)?) } else { (body, vtype) };
        match (key, vty) {
            (SPA_FORMAT_VIDEO_FORMAT, SPA_TYPE_ID) => f.format = rd(vbody)?,
            (SPA_FORMAT_VIDEO_SIZE, SPA_TYPE_RECTANGLE) => {
                f.width = rd(vbody)?;
                f.height = rd(vbody + 4)?;
            }
            _ => {}
        }
        o = body + vsize.div_ceil(8) * 8;
    }
    (f.width > 0 && f.height > 0).then_some(f)
}

// ---- libpipewire FFI --------------------------------------------------------

#[repr(C)]
struct SpaHook {
    _p: [*mut c_void; 8],
}

#[repr(C)]
struct StreamEvents {
    version: u32,
    destroy: Option<unsafe extern "C" fn(*mut c_void)>,
    state_changed: Option<unsafe extern "C" fn(*mut c_void, c_int, c_int, *const c_char)>,
    control_info: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    io_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *mut c_void, u32)>,
    param_changed: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void)>,
    add_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    remove_buffer: Option<unsafe extern "C" fn(*mut c_void, *mut c_void)>,
    process: Option<unsafe extern "C" fn(*mut c_void)>,
    drained: Option<unsafe extern "C" fn(*mut c_void)>,
    command: Option<unsafe extern "C" fn(*mut c_void, *const c_void)>,
    trigger_done: Option<unsafe extern "C" fn(*mut c_void)>,
}

#[repr(C)]
struct PwBuffer {
    buffer: *mut SpaBuffer,
}

#[repr(C)]
struct SpaBuffer {
    n_metas: u32,
    n_datas: u32,
    metas: *mut c_void,
    datas: *mut SpaData,
}

#[repr(C)]
struct SpaData {
    type_: u32,
    flags: u32,
    fd: i64,
    mapoffset: u32,
    maxsize: u32,
    data: *mut c_void,
    chunk: *mut SpaChunk,
}

#[repr(C)]
struct SpaChunk {
    offset: u32,
    size: u32,
    stride: i32,
    flags: i32,
}

struct Api {
    _lib: Library,
    init: unsafe extern "C" fn(*mut c_int, *mut *mut *mut c_char),
    thread_loop_new: unsafe extern "C" fn(*const c_char, *const c_void) -> *mut c_void,
    thread_loop_get_loop: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    thread_loop_start: unsafe extern "C" fn(*mut c_void) -> c_int,
    thread_loop_stop: unsafe extern "C" fn(*mut c_void),
    thread_loop_lock: unsafe extern "C" fn(*mut c_void),
    thread_loop_unlock: unsafe extern "C" fn(*mut c_void),
    context_new: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void,
    context_connect_fd: unsafe extern "C" fn(*mut c_void, c_int, *mut c_void, usize) -> *mut c_void,
    context_connect: unsafe extern "C" fn(*mut c_void, *mut c_void, usize) -> *mut c_void,
    properties_new: unsafe extern "C" fn(*const c_char, ...) -> *mut c_void,
    stream_new: unsafe extern "C" fn(*mut c_void, *const c_char, *mut c_void) -> *mut c_void,
    stream_add_listener: unsafe extern "C" fn(*mut c_void, *mut SpaHook, *const StreamEvents, *mut c_void),
    stream_connect: unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut *const c_void, u32) -> c_int,
    stream_dequeue_buffer: unsafe extern "C" fn(*mut c_void) -> *mut PwBuffer,
    stream_queue_buffer: unsafe extern "C" fn(*mut c_void, *mut PwBuffer) -> c_int,
    stream_update_params: unsafe extern "C" fn(*mut c_void, *mut *const c_void, u32) -> c_int,
    stream_disconnect: unsafe extern "C" fn(*mut c_void) -> c_int,
}

unsafe impl Send for Api {}
unsafe impl Sync for Api {}

fn api() -> Result<&'static Api> {
    static API: OnceLock<std::result::Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let lib = Library::new("libpipewire-0.3.so.0").map_err(|e| format!("cannot load libpipewire: {e}"))?;
        macro_rules! sym {
            ($n:literal) => {
                *lib.get($n).map_err(|e| format!("pipewire symbol {}: {e}", String::from_utf8_lossy($n)))?
            };
        }
        let api = Api {
            init: sym!(b"pw_init\0"),
            thread_loop_new: sym!(b"pw_thread_loop_new\0"),
            thread_loop_get_loop: sym!(b"pw_thread_loop_get_loop\0"),
            thread_loop_start: sym!(b"pw_thread_loop_start\0"),
            thread_loop_stop: sym!(b"pw_thread_loop_stop\0"),
            thread_loop_lock: sym!(b"pw_thread_loop_lock\0"),
            thread_loop_unlock: sym!(b"pw_thread_loop_unlock\0"),
            context_new: sym!(b"pw_context_new\0"),
            context_connect_fd: sym!(b"pw_context_connect_fd\0"),
            context_connect: sym!(b"pw_context_connect\0"),
            properties_new: sym!(b"pw_properties_new\0"),
            stream_new: sym!(b"pw_stream_new\0"),
            stream_add_listener: sym!(b"pw_stream_add_listener\0"),
            stream_connect: sym!(b"pw_stream_connect\0"),
            stream_dequeue_buffer: sym!(b"pw_stream_dequeue_buffer\0"),
            stream_queue_buffer: sym!(b"pw_stream_queue_buffer\0"),
            stream_update_params: sym!(b"pw_stream_update_params\0"),
            stream_disconnect: sym!(b"pw_stream_disconnect\0"),
            _lib: lib,
        };
        (api.init)(std::ptr::null_mut(), std::ptr::null_mut());
        Ok(api)
    })
    .as_ref()
    .map_err(|e| anyhow!("{e}"))
}

/// The most recent frame delivered by PipeWire.
pub struct Captured {
    pub format: VideoFormat,
    pub stride: usize,
    pub data: Vec<u8>,
    pub seq: u64,
    pub at: Instant,
}

struct Shared {
    stream: *mut c_void,
    format: Mutex<Option<VideoFormat>>,
    latest: Mutex<Option<Captured>>,
    cond: Condvar,
    state: Mutex<String>,
    frames: std::sync::atomic::AtomicU64,
}

unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

unsafe extern "C" fn on_state(data: *mut c_void, _old: c_int, new: c_int, err: *const c_char) {
    let s = &*(data as *const Shared);
    let name = match new {
        -1 => "error",
        0 => "unconnected",
        1 => "connecting",
        2 => "paused",
        3 => "streaming",
        _ => "?",
    };
    let msg = if err.is_null() { String::new() } else { std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned() };
    info!("pipewire stream {name} {msg}");
    *s.state.lock().unwrap() = name.to_string();
}

unsafe extern "C" fn on_param(data: *mut c_void, id: u32, param: *const c_void) {
    if id != SPA_PARAM_FORMAT || param.is_null() {
        return;
    }
    let s = &*(data as *const Shared);
    let size = *(param as *const u32) as usize + 8;
    let bytes = std::slice::from_raw_parts(param as *const u8, size);
    match parse_format(bytes) {
        Some(f) => {
            info!("pipewire format negotiated: {}x{} (spa format {})", f.width, f.height, f.format);
            *s.format.lock().unwrap() = Some(f);
            if let Ok(a) = api() {
                let b = Pod::shm_buffers();
                let mut params = [b.bytes().as_ptr() as *const c_void];
                (a.stream_update_params)(s.stream, params.as_mut_ptr(), 1);
            }
        }
        None => warn!("could not parse negotiated pipewire format"),
    }
}

unsafe extern "C" fn on_process(data: *mut c_void) {
    let s = &*(data as *const Shared);
    let Ok(a) = api() else { return };
    // Drain to the newest buffer.
    let mut newest: *mut PwBuffer = std::ptr::null_mut();
    loop {
        let b = (a.stream_dequeue_buffer)(s.stream);
        if b.is_null() {
            break;
        }
        if !newest.is_null() {
            (a.stream_queue_buffer)(s.stream, newest);
        }
        newest = b;
    }
    if newest.is_null() {
        return;
    }
    let buf = &*(*newest).buffer;
    if buf.n_datas > 0 && !buf.datas.is_null() {
        let d = &*buf.datas;
        let chunk = &*d.chunk;
        let fmt = *s.format.lock().unwrap();
        if let Some(fmt) = fmt {
            let stride = if chunk.stride > 0 { chunk.stride as usize } else { fmt.width as usize * 4 };
            let len = stride * fmt.height as usize;
            let mut mapped: *mut c_void = std::ptr::null_mut();
            let base = if !d.data.is_null() {
                d.data as *const u8
            } else if d.type_ == SPA_DATA_DMABUF && d.fd >= 0 {
                // Linear dma-bufs can be mapped directly.
                mapped = libc::mmap(std::ptr::null_mut(), d.maxsize as usize + d.mapoffset as usize, libc::PROT_READ, libc::MAP_SHARED, d.fd as c_int, 0);
                if mapped == libc::MAP_FAILED {
                    mapped = std::ptr::null_mut();
                    std::ptr::null()
                } else {
                    (mapped as *const u8).add(d.mapoffset as usize)
                }
            } else {
                std::ptr::null()
            };
            let off = chunk.offset as usize % (d.maxsize.max(1) as usize);
            if !base.is_null() && chunk.flags & SPA_CHUNK_FLAG_CORRUPTED == 0 && chunk.size > 0 && off + len <= d.maxsize as usize {
                let src = std::slice::from_raw_parts(base.add(off), len);
                let mut g = s.latest.lock().unwrap();
                let seq = s.frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                match g.as_mut() {
                    Some(c) if c.data.len() == len => {
                        c.data.copy_from_slice(src);
                        c.format = fmt;
                        c.stride = stride;
                        c.seq = seq;
                        c.at = Instant::now();
                    }
                    _ => *g = Some(Captured { format: fmt, stride, data: src.to_vec(), seq, at: Instant::now() }),
                }
                drop(g);
                s.cond.notify_all();
            }
            if !mapped.is_null() {
                libc::munmap(mapped, d.maxsize as usize + d.mapoffset as usize);
            }
        }
    }
    (a.stream_queue_buffer)(s.stream, newest);
}

static EVENTS: StreamEvents = StreamEvents {
    version: 2,
    destroy: None,
    state_changed: Some(on_state),
    control_info: None,
    io_changed: None,
    param_changed: Some(on_param),
    add_buffer: None,
    remove_buffer: None,
    process: Some(on_process),
    drained: None,
    command: None,
    trigger_done: None,
};

/// A connected PipeWire video stream.
pub struct PwCapture {
    tloop: *mut c_void,
    shared: Arc<Shared>,
    _hook: Box<SpaHook>,
    last_seq: u64,
}

unsafe impl Send for PwCapture {}
unsafe impl Sync for PwCapture {}

impl PwCapture {
    /// Connect to node `node_id`. `fd` is the portal's PipeWire remote; `None`
    /// connects to the default daemon.
    pub fn connect(fd: Option<OwnedFd>, node_id: Option<u32>, fps: u32) -> Result<Self> {
        let a = api()?;
        unsafe {
            let name = CString::new("2moon-capture")?;
            let tloop = (a.thread_loop_new)(name.as_ptr(), std::ptr::null());
            if tloop.is_null() {
                bail!("pw_thread_loop_new failed");
            }
            let ctx = (a.context_new)((a.thread_loop_get_loop)(tloop), std::ptr::null_mut(), 0);
            if ctx.is_null() {
                bail!("pw_context_new failed");
            }
            if (a.thread_loop_start)(tloop) < 0 {
                bail!("pw_thread_loop_start failed");
            }
            (a.thread_loop_lock)(tloop);
            let core = match fd {
                Some(fd) => (a.context_connect_fd)(ctx, fd.into_raw_fd(), std::ptr::null_mut(), 0),
                None => (a.context_connect)(ctx, std::ptr::null_mut(), 0),
            };
            if core.is_null() {
                (a.thread_loop_unlock)(tloop);
                bail!("cannot connect to PipeWire");
            }
            let k1 = CString::new("media.type")?;
            let v1 = CString::new("Video")?;
            let k2 = CString::new("media.category")?;
            let v2 = CString::new("Capture")?;
            let k3 = CString::new("media.role")?;
            let v3 = CString::new("Screen")?;
            let props = (a.properties_new)(k1.as_ptr(), v1.as_ptr(), k2.as_ptr(), v2.as_ptr(), k3.as_ptr(), v3.as_ptr(), std::ptr::null::<c_char>());
            let stream = (a.stream_new)(core, name.as_ptr(), props);
            if stream.is_null() {
                (a.thread_loop_unlock)(tloop);
                bail!("pw_stream_new failed");
            }
            let shared = Arc::new(Shared {
                stream,
                format: Mutex::new(None),
                latest: Mutex::new(None),
                cond: Condvar::new(),
                state: Mutex::new("unconnected".into()),
                frames: Default::default(),
            });
            let mut hook = Box::new(SpaHook { _p: [std::ptr::null_mut(); 8] });
            (a.stream_add_listener)(stream, &mut *hook, &EVENTS, Arc::as_ptr(&shared) as *mut c_void);
            let fmt = Pod::enum_format(fps);
            let mut params = [fmt.bytes().as_ptr() as *const c_void];
            let r = (a.stream_connect)(stream, PW_DIRECTION_INPUT, node_id.unwrap_or(PW_ID_ANY), PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS, params.as_mut_ptr(), 1);
            (a.thread_loop_unlock)(tloop);
            if r < 0 {
                bail!("pw_stream_connect failed ({r})");
            }
            debug!("pipewire stream connecting to node {node_id:?}");
            Ok(Self { tloop, shared, _hook: hook, last_seq: 0 })
        }
    }

    pub fn state(&self) -> String {
        self.shared.state.lock().unwrap().clone()
    }

    pub fn frames(&self) -> u64 {
        self.shared.frames.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Wait up to `timeout` for a frame newer than the last one returned and pass it to `f`.
    /// If nothing new arrives (static screen), the previous frame is reused.
    pub fn with_frame<R>(&mut self, timeout: Duration, f: impl FnOnce(&Captured) -> R) -> Option<R> {
        let mut g = self.shared.latest.lock().unwrap();
        let deadline = Instant::now() + timeout;
        while g.as_ref().map(|c| c.seq <= self.last_seq).unwrap_or(true) {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            g = self.shared.cond.wait_timeout(g, deadline - now).unwrap().0;
        }
        let c = g.as_ref()?;
        self.last_seq = c.seq;
        Some(f(c))
    }
}

impl Drop for PwCapture {
    fn drop(&mut self) {
        if let Ok(a) = api() {
            unsafe {
                (a.thread_loop_lock)(self.tloop);
                (a.stream_disconnect)(self.shared.stream);
                (a.thread_loop_unlock)(self.tloop);
                (a.thread_loop_stop)(self.tloop);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_roundtrip() {
        let p = Pod::enum_format(60);
        let b = p.bytes();
        assert_eq!(b.len() % 8, 0);
        assert_eq!(u32::from_ne_bytes(b[0..4].try_into().unwrap()) as usize, b.len() - 8);
        // A negotiated format with plain (non-choice) values.
        let mut f = Pod::default();
        let s = f.begin(SPA_TYPE_OBJECT);
        f.u32(SPA_TYPE_OBJECT_FORMAT);
        f.u32(SPA_PARAM_FORMAT);
        f.prop(SPA_FORMAT_MEDIA_TYPE);
        f.id(SPA_MEDIA_TYPE_VIDEO);
        f.prop(SPA_FORMAT_VIDEO_FORMAT);
        f.id(SPA_VIDEO_FORMAT_BGRX);
        f.prop(SPA_FORMAT_VIDEO_SIZE);
        f.u32(8);
        f.u32(SPA_TYPE_RECTANGLE);
        f.u32(2560);
        f.u32(1440);
        f.header_at(s);
        assert_eq!(parse_format(f.bytes()), Some(VideoFormat { format: SPA_VIDEO_FORMAT_BGRX, width: 2560, height: 1440 }));
        // Choice-wrapped size in the EnumFormat parses to its default.
        assert_eq!(parse_format(b).map(|f| (f.width, f.height)), Some((1920, 1080)));
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Needs a running PipeWire video producer whose node id is in MOON_PW_NODE, e.g.
    /// `gst-launch-1.0 videotestsrc is-live=true ! video/x-raw,format=BGRx,width=640,height=360,framerate=30/1 ! pipewiresink`
    #[test]
    #[ignore]
    fn capture_from_live_node() {
        let node: u32 = std::env::var("MOON_PW_NODE").expect("MOON_PW_NODE").parse().unwrap();
        let mut cap = PwCapture::connect(None, Some(node), 30).unwrap();
        let mut got = 0;
        let start = Instant::now();
        while got < 20 && start.elapsed() < Duration::from_secs(10) {
            if let Some((f, stride, nonblack)) = cap.with_frame(Duration::from_millis(500), |c| (c.format, c.stride, c.data.iter().any(|b| *b > 16))) {
                if got == 0 {
                    eprintln!("first frame: {f:?} stride {stride} state {}", cap.state());
                }
                assert_eq!((f.width, f.height), (640, 360));
                assert!(stride >= 640 * 4);
                assert!(nonblack);
                got += 1;
            }
        }
        eprintln!("received {} frames ({} callbacks) in {:?}", got, cap.frames(), start.elapsed());
        assert!(got >= 20, "only {got} frames");
    }
}
