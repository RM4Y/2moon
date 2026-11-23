//! Minimal PipeWire video consumer, loaded at run time (no headers).
//! Only the stable part of the API; format pods are built by hand.

use std::ffi::{c_char, c_int, c_void, CString};
use std::os::fd::{IntoRawFd, OwnedFd};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use libloading::Library;
use moon_media::glconv::{DmabufFrame, DmabufPlane};
use tracing::{debug, info, warn};

// ---- SPA constants -------------------------------------------------------
const SPA_TYPE_ID: u32 = 3;
const SPA_TYPE_INT: u32 = 4;
const SPA_TYPE_LONG: u32 = 5;
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
const SPA_FORMAT_VIDEO_MODIFIER: u32 = 0x20002;
const SPA_FORMAT_VIDEO_SIZE: u32 = 0x20003;
const SPA_FORMAT_VIDEO_FRAMERATE: u32 = 0x20004;
const SPA_FORMAT_VIDEO_MAX_FRAMERATE: u32 = 0x20005;
const SPA_POD_PROP_FLAG_MANDATORY: u32 = 1 << 3;
const SPA_POD_PROP_FLAG_DONT_FIXATE: u32 = 1 << 4;
const SPA_MEDIA_TYPE_VIDEO: u32 = 2;
const SPA_MEDIA_SUBTYPE_RAW: u32 = 1;
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
        self.prop_flags(key, 0);
    }
    fn prop_flags(&mut self, key: u32, flags: u32) {
        self.u32(key);
        self.u32(flags);
    }
    fn fraction(&mut self, num: u32, den: u32) {
        self.u32(8);
        self.u32(SPA_TYPE_FRACTION);
        self.u32(num);
        self.u32(den);
    }
    /// An enum choice of 64-bit values (the first is the default).
    fn choice_long(&mut self, values: &[u64]) {
        let s = self.begin(SPA_TYPE_CHOICE);
        self.u32(SPA_CHOICE_ENUM);
        self.u32(0);
        self.u32(8);
        self.u32(SPA_TYPE_LONG);
        self.0.extend_from_slice(&values[0].to_ne_bytes());
        for v in values {
            self.0.extend_from_slice(&v.to_ne_bytes());
        }
        self.header_at(s);
        self.pad();
    }

    /// Raw video EnumFormat: DMA-BUF with `modifiers`, else 32-bit RGB in memory.
    /// Variable frame rate capped at `fps`, so each picture comes as soon as rendered.
    fn video_format(format: Option<u32>, modifiers: &[u64], fps: u32) -> Self {
        let mut p = Pod::default();
        let s = p.begin(SPA_TYPE_OBJECT);
        p.u32(SPA_TYPE_OBJECT_FORMAT);
        p.u32(SPA_PARAM_ENUM_FORMAT);
        p.prop(SPA_FORMAT_MEDIA_TYPE);
        p.id(SPA_MEDIA_TYPE_VIDEO);
        p.prop(SPA_FORMAT_MEDIA_SUBTYPE);
        p.id(SPA_MEDIA_SUBTYPE_RAW);
        p.prop(SPA_FORMAT_VIDEO_FORMAT);
        match format {
            Some(f) => p.id(f),
            None => p.choice(SPA_CHOICE_ENUM, 4, SPA_TYPE_ID, &[SPA_VIDEO_FORMAT_BGRX, SPA_VIDEO_FORMAT_BGRX, SPA_VIDEO_FORMAT_BGRA, SPA_VIDEO_FORMAT_RGBX, SPA_VIDEO_FORMAT_RGBA]),
        }
        if !modifiers.is_empty() {
            p.prop_flags(SPA_FORMAT_VIDEO_MODIFIER, SPA_POD_PROP_FLAG_MANDATORY | SPA_POD_PROP_FLAG_DONT_FIXATE);
            p.choice_long(modifiers);
        }
        p.prop(SPA_FORMAT_VIDEO_SIZE);
        p.choice(SPA_CHOICE_RANGE, 8, SPA_TYPE_RECTANGLE, &[1920, 1080, 1, 1, 8192, 8192]);
        p.prop(SPA_FORMAT_VIDEO_FRAMERATE);
        p.fraction(0, 1);
        // Prefer every picture the compositor renders: KWin's own rate limiter is
        // coarse (whole milliseconds on a Qt timer) and turns 120 Hz into ~57 fps
        // for a 60 fps cap; the encoder loop paces to the client's rate instead.
        let _ = fps;
        p.prop(SPA_FORMAT_VIDEO_MAX_FRAMERATE);
        p.choice(SPA_CHOICE_RANGE, 8, SPA_TYPE_FRACTION, &[1000, 1, 0, 1, 1000, 1]);
        p.header_at(s);
        p
    }

    /// EnumFormat for memory buffers (CPU copies).
    pub fn enum_format(fps: u32) -> Self {
        Self::video_format(None, &[], fps)
    }

    /// EnumFormat for DMA-BUFs of one format with the given DRM modifiers.
    pub fn enum_format_dmabuf(format: u32, modifiers: &[u64], fps: u32) -> Self {
        Self::video_format(Some(format), modifiers, fps)
    }

    /// Buffers param asking for DMA-BUFs.
    pub fn dmabuf_buffers() -> Self {
        let mut p = Pod::default();
        let s = p.begin(SPA_TYPE_OBJECT);
        p.u32(SPA_TYPE_OBJECT_PARAM_BUFFERS);
        p.u32(SPA_PARAM_BUFFERS);
        p.prop(SPA_PARAM_BUFFERS_DATATYPE);
        p.choice(SPA_CHOICE_FLAGS, 4, SPA_TYPE_INT, &[1 << SPA_DATA_DMABUF]);
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
    /// DRM modifier: present when the buffers are DMA-BUFs.
    pub modifier: Option<u64>,
    /// Negotiated maximum frame rate (num, den).
    pub max_framerate: (u32, u32),
}

/// DRM fourcc matching an SPA RGB format (the byte orders are named the other way round).
pub fn drm_fourcc(spa: u32) -> Option<u32> {
    use moon_media::glconv::*;
    Some(match spa {
        SPA_VIDEO_FORMAT_BGRX => DRM_FORMAT_XRGB8888,
        SPA_VIDEO_FORMAT_BGRA => DRM_FORMAT_ARGB8888,
        SPA_VIDEO_FORMAT_RGBX => DRM_FORMAT_XBGR8888,
        SPA_VIDEO_FORMAT_RGBA => DRM_FORMAT_ABGR8888,
        _ => return None,
    })
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
            (SPA_FORMAT_VIDEO_MODIFIER, SPA_TYPE_LONG) => f.modifier = Some(rd(vbody)? as u64 | (rd(vbody + 4)? as u64) << 32),
            (SPA_FORMAT_VIDEO_MAX_FRAMERATE, SPA_TYPE_FRACTION) => f.max_framerate = (rd(vbody)?, rd(vbody + 4)?),
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

/// The most recent frame delivered by PipeWire in memory buffers.
pub struct Captured {
    pub format: VideoFormat,
    pub stride: usize,
    pub data: Vec<u8>,
    pub seq: u64,
    pub at: Instant,
}

/// DMA-BUF buffers: the newest one is kept (not handed back to the compositor)
/// until a newer one arrives, and the one being converted is never handed back
/// before the conversion has finished reading it.
struct DmaState {
    current: *mut PwBuffer,
    frame: Option<DmabufFrame>,
    busy: *mut PwBuffer,
    /// The busy buffer was removed by PipeWire meanwhile: do not queue it back.
    busy_removed: bool,
    seq: u64,
    at: Option<Instant>,
    /// Bumped when buffers are renegotiated (imported buffers must be forgotten).
    generation: u64,
}

struct Shared {
    stream: *mut c_void,
    format: Mutex<Option<VideoFormat>>,
    latest: Mutex<Option<Captured>>,
    cond: Condvar,
    dma: Mutex<DmaState>,
    dma_cond: Condvar,
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
            let (n, d) = f.max_framerate;
            info!(
                "pipewire format negotiated: {}x{} (spa format {}), {}, max {} fps",
                f.width,
                f.height,
                f.format,
                match f.modifier {
                    Some(m) => format!("DMA-BUF modifier 0x{m:x}"),
                    None => "memory buffers".into(),
                },
                if d > 0 { format!("{:.0}", n as f64 / d as f64) } else { "?".into() }
            );
            *s.format.lock().unwrap() = Some(f);
            {
                let mut dma = s.dma.lock().unwrap();
                dma.current = std::ptr::null_mut();
                dma.frame = None;
                dma.generation += 1;
            }
            if let Ok(a) = api() {
                let b = if f.modifier.is_some() { Pod::dmabuf_buffers() } else { Pod::shm_buffers() };
                let mut params = [b.bytes().as_ptr() as *const c_void];
                (a.stream_update_params)(s.stream, params.as_mut_ptr(), 1);
            }
        }
        None => warn!("could not parse negotiated pipewire format"),
    }
}

unsafe extern "C" fn on_remove_buffer(data: *mut c_void, b: *mut c_void) {
    let s = &*(data as *const Shared);
    let b = b as *mut PwBuffer;
    let mut dma = s.dma.lock().unwrap();
    if dma.current == b {
        dma.current = std::ptr::null_mut();
        dma.frame = None;
    }
    if dma.busy == b {
        dma.busy_removed = true;
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
    if buf.n_datas == 0 || buf.datas.is_null() {
        (a.stream_queue_buffer)(s.stream, newest);
        return;
    }
    let d = &*buf.datas;
    let chunk = &*d.chunk;
    let fmt = *s.format.lock().unwrap();
    let Some(fmt) = fmt else {
        (a.stream_queue_buffer)(s.stream, newest);
        return;
    };
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        info!("pipewire buffers: data type {} ({}), {} plane(s), stride {}", d.type_, match d.type_ { 1 => "MemPtr", 2 => "MemFd", 3 => "DmaBuf", _ => "?" }, buf.n_datas, chunk.stride);
    }
    if chunk.flags & SPA_CHUNK_FLAG_CORRUPTED != 0 {
        (a.stream_queue_buffer)(s.stream, newest);
        return;
    }

    // GPU path: keep the buffer, the encoder thread imports it.
    if d.type_ == SPA_DATA_DMABUF {
        if let (Some(modifier), Some(fourcc)) = (fmt.modifier, drm_fourcc(fmt.format)) {
            let datas = std::slice::from_raw_parts(buf.datas, (buf.n_datas as usize).min(4));
            let planes = datas.iter().map(|p| DmabufPlane { fd: p.fd as i32, offset: (*p.chunk).offset, stride: (*p.chunk).stride.max(0) as u32 }).collect();
            let mut dma = s.dma.lock().unwrap();
            let frame = DmabufFrame { width: fmt.width, height: fmt.height, fourcc, modifier, planes, key: (newest as usize) ^ ((dma.generation as usize) << 48) };
            let old = std::mem::replace(&mut dma.current, newest);
            if !old.is_null() && old != dma.busy && old != newest {
                (a.stream_queue_buffer)(s.stream, old);
            }
            dma.frame = Some(frame);
            dma.seq += 1;
            dma.at = Some(Instant::now());
            drop(dma);
            s.frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            s.dma_cond.notify_all();
            return;
        }
    }

    // CPU path: copy the picture out and hand the buffer back at once.
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
    if !base.is_null() && chunk.size > 0 && off + len <= d.maxsize as usize {
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
    remove_buffer: Some(on_remove_buffer),
    process: Some(on_process),
    drained: None,
    command: None,
    trigger_done: None,
};

/// A connected PipeWire video stream.
pub struct PwCapture {
    tloop: *mut c_void,
    shared: Arc<Shared>,
    /// Node the stream is connected to (`PW_ID_ANY`: chosen by PipeWire).
    node: u32,
    _hook: Box<SpaHook>,
    last_seq: u64,
    last_dma_seq: u64,
}

unsafe impl Send for PwCapture {}
unsafe impl Sync for PwCapture {}

impl PwCapture {
    /// Connect to node `node_id`. `fd` is the portal's PipeWire remote; `None`
    /// connects to the default daemon. `dmabuf` lists (SPA format, DRM modifiers)
    /// the GPU path can import; they are preferred over memory buffers.
    pub fn connect(fd: Option<OwnedFd>, node_id: Option<u32>, fps: u32, dmabuf: &[(u32, Vec<u64>)]) -> Result<Self> {
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
                dma: Mutex::new(DmaState { current: std::ptr::null_mut(), frame: None, busy: std::ptr::null_mut(), busy_removed: false, seq: 0, at: None, generation: 0 }),
                dma_cond: Condvar::new(),
                state: Mutex::new("unconnected".into()),
                frames: Default::default(),
            });
            let mut hook = Box::new(SpaHook { _p: [std::ptr::null_mut(); 8] });
            (a.stream_add_listener)(stream, &mut *hook, &EVENTS, Arc::as_ptr(&shared) as *mut c_void);
            let node = node_id.unwrap_or(PW_ID_ANY);
            let r = stream_connect(a, stream, node, fps, dmabuf);
            (a.thread_loop_unlock)(tloop);
            if r < 0 {
                bail!("pw_stream_connect failed ({r})");
            }
            debug!("pipewire stream connecting to node {node_id:?}");
            Ok(Self { tloop, shared, node, _hook: hook, last_seq: 0, last_dma_seq: 0 })
        }
    }

    /// Negotiate the format again on the same node and PipeWire connection (a
    /// portal capture keeps its permission: no new dialog). `dmabuf` as for
    /// [`Self::connect`]; empty: memory buffers only.
    pub fn renegotiate(&mut self, fps: u32, dmabuf: &[(u32, Vec<u64>)]) -> Result<()> {
        let a = api()?;
        unsafe {
            (a.thread_loop_lock)(self.tloop);
            (a.stream_disconnect)(self.shared.stream);
            *self.shared.format.lock().unwrap() = None;
            *self.shared.latest.lock().unwrap() = None;
            {
                let mut dma = self.shared.dma.lock().unwrap();
                dma.current = std::ptr::null_mut();
                dma.frame = None;
                dma.busy = std::ptr::null_mut();
                dma.busy_removed = false;
                dma.generation += 1;
            }
            let r = stream_connect(a, self.shared.stream, self.node, fps, dmabuf);
            (a.thread_loop_unlock)(self.tloop);
            if r < 0 {
                bail!("pw_stream_connect failed ({r})");
            }
        }
        info!("pipewire: renegotiating ({})", if dmabuf.iter().any(|(_, m)| !m.is_empty()) { "DMA-BUF preferred" } else { "memory buffers" });
        Ok(())
    }

    pub fn state(&self) -> String {
        self.shared.state.lock().unwrap().clone()
    }

    pub fn frames(&self) -> u64 {
        self.shared.frames.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The negotiated format, once known.
    pub fn format(&self) -> Option<VideoFormat> {
        *self.shared.format.lock().unwrap()
    }

    /// Frames come as DMA-BUFs (GPU path).
    pub fn is_dmabuf(&self) -> bool {
        self.format().is_some_and(|f| f.modifier.is_some())
    }

    /// Wait until `deadline` for a picture newer than the last one used; true if there is one.
    pub fn wait_new(&self, deadline: Instant) -> bool {
        if self.is_dmabuf() {
            let mut g = self.shared.dma.lock().unwrap();
            while g.seq <= self.last_dma_seq || g.frame.is_none() {
                let now = Instant::now();
                if now >= deadline {
                    return false;
                }
                g = self.shared.dma_cond.wait_timeout(g, deadline - now).unwrap().0;
            }
            true
        } else {
            let mut g = self.shared.latest.lock().unwrap();
            while g.as_ref().map(|c| c.seq <= self.last_seq).unwrap_or(true) {
                let now = Instant::now();
                if now >= deadline {
                    // The negotiation may have switched to DMA-BUFs meanwhile.
                    return false;
                }
                g = self.shared.cond.wait_timeout(g, (deadline - now).min(Duration::from_millis(50))).unwrap().0;
                if self.is_dmabuf() {
                    return false;
                }
            }
            true
        }
    }

    /// Run `f` on the newest DMA-BUF picture if it is newer than the last one used.
    /// The buffer stays out of the compositor's hands while `f` runs. The second
    /// value of `f`'s argument is the buffer generation (changes on renegotiation).
    pub fn with_dmabuf<R>(&mut self, f: impl FnOnce(&DmabufFrame, u64) -> R) -> Option<R> {
        let (buf, frame, generation) = {
            let mut g = self.shared.dma.lock().unwrap();
            if g.seq <= self.last_dma_seq || g.current.is_null() {
                return None;
            }
            let frame = g.frame.clone()?;
            self.last_dma_seq = g.seq;
            g.busy = g.current;
            g.busy_removed = false;
            (g.current, frame, g.generation)
        };
        let r = f(&frame, generation);
        let Ok(a) = api() else { return Some(r) };
        unsafe {
            (a.thread_loop_lock)(self.tloop);
            let mut g = self.shared.dma.lock().unwrap();
            if g.busy == buf {
                g.busy = std::ptr::null_mut();
                if buf != g.current && !g.busy_removed {
                    (a.stream_queue_buffer)(self.shared.stream, buf);
                }
            }
            drop(g);
            (a.thread_loop_unlock)(self.tloop);
        }
        Some(r)
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
        let c = g.as_ref().filter(|c| c.seq > self.last_seq)?;
        self.last_seq = c.seq;
        Some(f(c))
    }

    /// When the newest picture not used yet arrived (None: nothing new).
    pub fn pending_arrival(&self) -> Option<Instant> {
        if self.is_dmabuf() {
            let g = self.shared.dma.lock().unwrap();
            if g.seq > self.last_dma_seq && g.frame.is_some() { g.at } else { None }
        } else {
            let g = self.shared.latest.lock().unwrap();
            g.as_ref().filter(|c| c.seq > self.last_seq).map(|c| c.at)
        }
    }

    /// Wait until a picture not used yet that arrived at or after `after` is there,
    /// at the latest until `deadline`; true if there is one.
    pub fn wait_arrival_after(&self, after: Instant, deadline: Instant) -> bool {
        loop {
            if self.pending_arrival().is_some_and(|at| at >= after) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let left = (deadline - now).min(Duration::from_millis(50));
            if self.is_dmabuf() {
                let g = self.shared.dma.lock().unwrap();
                if g.at.is_some_and(|at| at >= after) && g.seq > self.last_dma_seq {
                    continue;
                }
                drop(self.shared.dma_cond.wait_timeout(g, left).unwrap());
            } else {
                let g = self.shared.latest.lock().unwrap();
                if g.as_ref().is_some_and(|c| c.at >= after && c.seq > self.last_seq) {
                    continue;
                }
                drop(self.shared.cond.wait_timeout(g, left).unwrap());
            }
        }
    }

    /// Arrival times of new pictures, for diagnostics.
    pub fn next_arrival(&mut self, timeout: Duration) -> Option<Instant> {
        if !self.wait_new(Instant::now() + timeout) {
            return None;
        }
        if self.is_dmabuf() {
            let mut g = self.shared.dma.lock().unwrap();
            self.last_dma_seq = g.seq;
            let at = g.at;
            // Diagnostics only: hand the buffer back at once.
            if !g.current.is_null() && g.busy.is_null() {
                if let Ok(a) = api() {
                    let b = std::mem::replace(&mut g.current, std::ptr::null_mut());
                    g.frame = None;
                    drop(g);
                    unsafe {
                        (a.thread_loop_lock)(self.tloop);
                        (a.stream_queue_buffer)(self.shared.stream, b);
                        (a.thread_loop_unlock)(self.tloop);
                    }
                }
            }
            at
        } else {
            self.with_frame(Duration::ZERO, |c| c.at)
        }
    }
}

/// Connect `stream` to `node` offering the DMA-BUF formats first, then memory
/// buffers. Call with the thread loop locked.
unsafe fn stream_connect(a: &Api, stream: *mut c_void, node: u32, fps: u32, dmabuf: &[(u32, Vec<u64>)]) -> c_int {
    let mut pods: Vec<Pod> = dmabuf.iter().filter(|(_, m)| !m.is_empty()).map(|(f, m)| Pod::enum_format_dmabuf(*f, m, fps)).collect();
    pods.push(Pod::enum_format(fps));
    let mut params: Vec<*const c_void> = pods.iter().map(|p| p.bytes().as_ptr() as *const c_void).collect();
    (a.stream_connect)(stream, PW_DIRECTION_INPUT, node, PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS, params.as_mut_ptr(), params.len() as u32)
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
        assert_eq!(parse_format(f.bytes()), Some(VideoFormat { format: SPA_VIDEO_FORMAT_BGRX, width: 2560, height: 1440, ..Default::default() }));
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
        let mut cap = PwCapture::connect(None, Some(node), 30, &[]).unwrap();
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
