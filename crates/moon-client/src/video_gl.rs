//! GPU video presentation for the egui client, drawn through an egui paint
//! callback, with the YUV -> RGB conversion in a fragment shader.
//!
//! Two input paths:
//! - CPU pictures (I420): the three planes are uploaded as single-channel textures
//!   (1.5 bytes/pixel instead of 4 for RGBA, and no CPU colour conversion);
//! - zero-copy VAAPI pictures (NV12 surfaces still on the GPU): exported as
//!   DMA-BUF and imported as EGL images, so the picture never leaves video
//!   memory, as Moonlight does. If the import fails (no EGL, no dma-buf import,
//!   unsupported format), zero-copy is switched off for good and the decoder
//!   falls back to copying pictures to system memory.

use std::collections::VecDeque;
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui;
use eframe::egui_glow;
use eframe::glow::{self, HasContext};
use moon_media::decoder::Decoded;
use moon_media::ffenc::VaapiFrame;
use moon_media::frame::{Csc, I420Frame};
use moon_media::hdr::{HdrMetadata, Yuv16Frame};
use moon_media::nvdec::{CudaFrame, CudaGlTarget};
use tracing::{info, warn};

enum Pending {
    Cpu(I420Frame),
    Cpu16(Yuv16Frame),
    Vaapi(VaapiFrame),
    Cuda(CudaFrame),
}

/// Frames exchanged between the decode thread and the paint callback.
#[derive(Default)]
struct Exchange {
    /// Stream generation: frames from an older stream are ignored.
    generation: u64,
    /// Decoded frames not yet presented, oldest first. Like Moonlight's frame
    /// pacing: Wi-Fi often delivers two frames at once, and showing only the
    /// newest would halve the frame rate; they are shown one per frame interval.
    queue: VecDeque<Pending>,
    /// Frame interval of the stream, and when the last frame was presented.
    interval: Duration,
    last_present: Option<Instant>,
    /// Frame pacing on: frames of a burst are queued and shown one interval apart.
    /// Off (minimum latency): only the newest frame is kept and shown at once.
    pacing: bool,
    /// Frames dropped from the queue before being shown (display too slow).
    replaced: u64,
    /// A CPU buffer to recycle for the next copy.
    spare: Option<I420Frame>,
    spare16: Option<Yuv16Frame>,
    /// HDR content peak relative to SDR white (from the host's HDR metadata).
    hdr_peak: f32,
    /// Size of the latest frame (0 before the first one).
    size: [usize; 2],
    /// Frames uploaded to the GPU (= frames presented).
    presented: u64,
    /// Frame index of each queued frame and when it was decoded (same order as `queue`).
    queue_index: VecDeque<(u32, Instant)>,
    /// Frames put on screen since the last `take_shown`.
    shown: Vec<Shown>,
}

/// A frame put on screen.
#[derive(Clone, Copy, Debug)]
pub struct Shown {
    pub index: u32,
    /// When the decoder handed it over.
    pub decoded: Instant,
    /// When it was drawn.
    pub at: Instant,
}

/// Frames kept waiting for display at most.
const MAX_QUEUED: usize = 2;
/// A queued frame is shown once this fraction of the frame interval has passed
/// since the previous one (a little early, to absorb timer and vsync slack).
const PACE: f64 = 0.85;

#[derive(Clone)]
pub struct VideoView {
    ex: Arc<Mutex<Exchange>>,
    gl: Arc<Mutex<Option<Renderer>>>,
    /// Shared with the decoder: VAAPI pictures stay on the GPU while true.
    zero_copy: Arc<AtomicBool>,
}

impl Default for VideoView {
    fn default() -> Self {
        Self { ex: Default::default(), gl: Default::default(), zero_copy: Arc::new(AtomicBool::new(zero_copy_allowed())) }
    }
}

/// Zero-copy unless disabled (MOON_ZERO_COPY=0) or found not to work earlier.
fn zero_copy_allowed() -> bool {
    std::env::var("MOON_ZERO_COPY").as_deref() != Ok("0") && !ZERO_COPY_BROKEN.load(Ordering::Relaxed)
}

static ZERO_COPY_BROKEN: AtomicBool = AtomicBool::new(false);

impl VideoView {
    /// Start a new stream at `fps`: drop previous frames, return the sink generation.
    pub fn reset(&self, fps: u32, pacing: bool) -> u64 {
        let mut ex = self.ex.lock().unwrap();
        ex.pacing = pacing;
        ex.interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
        ex.generation += 1;
        ex.queue.clear();
        ex.queue_index.clear();
        ex.shown.clear();
        ex.size = [0, 0];
        ex.presented = 0;
        ex.replaced = 0;
        ex.last_present = None;
        ex.hdr_peak = 1000.0 / moon_media::hdr::SDR_WHITE_NITS;
        self.zero_copy.store(zero_copy_allowed(), Ordering::Relaxed);
        ex.generation
    }

    /// The host's HDR mode message: use its metadata for tone mapping.
    pub fn set_hdr(&self, metadata: Option<HdrMetadata>) {
        if let Some(m) = metadata {
            self.ex.lock().unwrap().hdr_peak = m.peak();
        }
    }

    /// The flag to give the decoder (see `SessionOptions::zero_copy`).
    pub fn zero_copy_flag(&self) -> Arc<AtomicBool> {
        self.zero_copy.clone()
    }

    /// Called on the decode thread for every decoded picture.
    pub fn put(&self, generation: u64, d: Decoded<'_>, index: u32) {
        let (pending, size) = match d {
            Decoded::Cpu(f) => {
                let mut buf = {
                    let mut ex = self.ex.lock().unwrap();
                    if ex.generation != generation {
                        return;
                    }
                    ex.spare.take().unwrap_or_else(|| I420Frame::new(2, 2))
                };
                buf.copy_from(f);
                (Pending::Cpu(buf), [f.width, f.height])
            }
            Decoded::Cpu16(f) => {
                let mut buf = {
                    let mut ex = self.ex.lock().unwrap();
                    if ex.generation != generation {
                        return;
                    }
                    ex.spare16.take().unwrap_or_else(|| Yuv16Frame::new(2, 2))
                };
                buf.copy_from(f);
                (Pending::Cpu16(buf), [f.width, f.height])
            }
            Decoded::Vaapi(v) => {
                let size = [v.width, v.height];
                (Pending::Vaapi(v), size)
            }
            Decoded::Cuda(c) => {
                let size = [c.width, c.height];
                (Pending::Cuda(c), size)
            }
        };
        let mut ex = self.ex.lock().unwrap();
        if ex.generation != generation {
            return;
        }
        ex.size = size;
        ex.queue.push_back(pending);
        ex.queue_index.push_back((index, Instant::now()));
        // More than two waiting: the display is really behind, drop the oldest
        // (bounded latency: at most one extra frame interval).
        let max = if ex.pacing { MAX_QUEUED } else { 1 };
        while ex.queue.len() > max {
            ex.replaced += 1;
            ex.queue_index.pop_front();
            match ex.queue.pop_front() {
                Some(Pending::Cpu(old)) => ex.spare = Some(old),
                Some(Pending::Cpu16(old)) => ex.spare16 = Some(old),
                _ => {}
            }
        }
    }

    /// Frames dropped before being shown because the display could not keep up.
    pub fn replaced(&self) -> u64 {
        self.ex.lock().unwrap().replaced
    }

    /// If frames are waiting, how long until the next one is due (ask for a repaint then).
    pub fn next_due(&self) -> Option<Duration> {
        let ex = self.ex.lock().unwrap();
        if ex.queue.is_empty() {
            return None;
        }
        Some(match ex.last_present {
            Some(_) if !ex.pacing => Duration::ZERO,
            Some(t) => (t + ex.interval.mul_f64(PACE)).saturating_duration_since(Instant::now()),
            None => Duration::ZERO,
        })
    }

    /// The next frame to present, if one is due.
    fn take_due(&self) -> Option<Pending> {
        let mut ex = self.ex.lock().unwrap();
        let due = match ex.last_present {
            None => true,
            Some(_) if !ex.pacing => true,
            // A frame that arrives on time is shown at once (no added latency);
            // the second frame of a burst waits for its turn.
            Some(t) => t.elapsed() >= ex.interval.mul_f64(PACE) || ex.queue.len() >= MAX_QUEUED,
        };
        if !due {
            return None;
        }
        let f = ex.queue.pop_front()?;
        let now = Instant::now();
        ex.last_present = Some(now);
        if let Some((index, decoded)) = ex.queue_index.pop_front() {
            if ex.shown.len() < 64 {
                ex.shown.push(Shown { index, decoded, at: now });
            }
        }
        Some(f)
    }

    /// Frames put on screen since the last call.
    pub fn take_shown(&self) -> Vec<Shown> {
        std::mem::take(&mut self.ex.lock().unwrap().shown)
    }

    pub fn size(&self) -> [usize; 2] {
        self.ex.lock().unwrap().size
    }

    pub fn presented(&self) -> u64 {
        self.ex.lock().unwrap().presented
    }

    /// Paint the video into `rect`.
    pub fn paint(&self, painter: &egui::Painter, rect: egui::Rect) {
        let view = self.clone();
        let cb = egui_glow::CallbackFn::new(move |_info, p| view.gl_paint(p));
        painter.add(egui::PaintCallback { rect, callback: Arc::new(cb) });
    }

    fn gl_paint(&self, p: &egui_glow::Painter) {
        let gl = p.gl();
        let mut r = self.gl.lock().unwrap();
        if r.is_none() {
            match unsafe { Renderer::new(gl) } {
                Ok(n) => *r = Some(n),
                Err(e) => {
                    warn!("video renderer: {e}");
                    return;
                }
            }
        }
        let r = r.as_mut().unwrap();
        let frame = self.take_due();
        match frame {
            Some(Pending::Cpu(f)) => {
                unsafe { r.upload(gl, &f) };
                let mut ex = self.ex.lock().unwrap();
                ex.presented += 1;
                ex.spare = Some(f);
            }
            Some(Pending::Cpu16(f)) => {
                let peak = self.ex.lock().unwrap().hdr_peak;
                unsafe { r.upload16(gl, &f, peak) };
                let mut ex = self.ex.lock().unwrap();
                ex.presented += 1;
                ex.spare16 = Some(f);
            }
            Some(Pending::Vaapi(v)) => match unsafe { r.import_vaapi(gl, v) } {
                Ok(()) => self.ex.lock().unwrap().presented += 1,
                Err(e) => self.zero_copy_failed(&e),
            },
            Some(Pending::Cuda(c)) => match unsafe { r.import_cuda(gl, &c) } {
                Ok(()) => self.ex.lock().unwrap().presented += 1,
                Err(e) => self.zero_copy_failed(&e),
            },
            None => {}
        }
        unsafe { r.draw(gl) };
    }

    fn zero_copy_failed(&self, e: &str) {
        warn!("zero-copy display unavailable ({e}); copying pictures instead");
        ZERO_COPY_BROKEN.store(true, Ordering::Relaxed);
        self.zero_copy.store(false, Ordering::Relaxed);
    }

    /// Free GL resources (call with the context current, e.g. from `on_exit`).
    pub fn destroy(&self, gl: &glow::Context) {
        if let Some(r) = self.gl.lock().unwrap().take() {
            unsafe { r.destroy(gl) };
        }
    }
}

// ------------------------------------------------------------------ EGL

const EGL_EXTENSIONS: i32 = 0x3055;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_NONE: i32 = 0x3038;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

type CreateImage = unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> *mut c_void;
type DestroyImage = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type ImageTargetTexture = unsafe extern "C" fn(u32, *mut c_void);

struct Egl {
    _lib: libloading::Library,
    get_current_display: unsafe extern "C" fn() -> *mut c_void,
    query_string: unsafe extern "C" fn(*mut c_void, i32) -> *const c_char,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    image_target_texture: ImageTargetTexture,
}

fn egl() -> Result<&'static Egl, String> {
    static EGL: OnceLock<Result<Egl, String>> = OnceLock::new();
    EGL.get_or_init(|| unsafe {
        let lib = libloading::Library::new("libEGL.so.1").map_err(|e| format!("no libEGL: {e}"))?;
        let get_proc: unsafe extern "C" fn(*const c_char) -> *mut c_void = *lib.get(b"eglGetProcAddress\0").map_err(|e| e.to_string())?;
        let proc = |n: &[u8]| -> Result<*mut c_void, String> {
            let p = get_proc(n.as_ptr() as *const c_char);
            if p.is_null() {
                Err(format!("EGL has no {}", String::from_utf8_lossy(&n[..n.len() - 1])))
            } else {
                Ok(p)
            }
        };
        Ok(Egl {
            get_current_display: *lib.get(b"eglGetCurrentDisplay\0").map_err(|e| e.to_string())?,
            query_string: *lib.get(b"eglQueryString\0").map_err(|e| e.to_string())?,
            create_image: std::mem::transmute::<*mut c_void, CreateImage>(proc(b"eglCreateImageKHR\0")?),
            destroy_image: std::mem::transmute::<*mut c_void, DestroyImage>(proc(b"eglDestroyImageKHR\0")?),
            image_target_texture: std::mem::transmute::<*mut c_void, ImageTargetTexture>(proc(b"glEGLImageTargetTexture2DOES\0")?),
            _lib: lib,
        })
    })
    .as_ref()
    .map_err(|e| e.clone())
}

/// Pictures imported from VAAPI: the surface (kept alive while displayed) and its EGL images.
struct Imported {
    _frame: VaapiFrame,
    display: *mut c_void,
    images: Vec<*mut c_void>,
}

// ------------------------------------------------------------------ renderer

struct Program {
    program: glow::Program,
    u_csc: Option<glow::UniformLocation>,
    u_off: Option<glow::UniformLocation>,
    u_scale: Option<glow::UniformLocation>,
    csc: Option<Csc>,
}

enum Source {
    None,
    /// Three planes uploaded from system memory.
    I420,
    /// NV12 EGL images (luma + interleaved chroma); `scale` crops the surface padding.
    Nv12 { scale: [f32; 2] },
    /// NV12 copied by CUDA into `tex_cuda`.
    Cuda,
    /// 10-bit HDR10 planes in `tex16`, tone-mapped to SDR.
    Hdr { peak: f32 },
}

struct Renderer {
    i420: Program,
    nv12: Option<Program>,
    /// HDR10 -> SDR (GL3 / GLES3).
    hdr: Option<Program>,
    u_peak: Option<glow::UniformLocation>,
    u_scale10: Option<glow::UniformLocation>,
    /// 16-bit planes (or 8-bit on GLES without 16-bit textures).
    tex16: [glow::Texture; 3],
    size16: [usize; 2],
    /// R16 textures are usable (desktop OpenGL).
    norm16: bool,
    vbo: glow::Buffer,
    vao: Option<glow::VertexArray>,
    a_pos: u32,
    tex: [glow::Texture; 3],
    /// Textures bound to EGL images (Y, UV).
    tex_nv12: [glow::Texture; 2],
    /// Textures written by CUDA (Y R8, UV RG8) and their CUDA registration.
    tex_cuda: [glow::Texture; 2],
    cuda: Option<CudaGlTarget>,
    size: [usize; 2],
    /// Chroma format of the I420 textures.
    yuv444: bool,
    csc: Csc,
    source: Source,
    imported: Option<Imported>,
    /// R8/RED textures (GL3 / GLES3) or LUMINANCE (GL2 / GLES2).
    modern: bool,
}

unsafe impl Send for Renderer {}

unsafe fn compile(gl: &glow::Context, vs: &str, fs: &str, samplers: &[&str]) -> Result<Program, String> {
    let program = gl.create_program()?;
    let mut shaders = Vec::new();
    for (kind, src) in [(glow::VERTEX_SHADER, vs), (glow::FRAGMENT_SHADER, fs)] {
        let s = gl.create_shader(kind)?;
        gl.shader_source(s, src);
        gl.compile_shader(s);
        if !gl.get_shader_compile_status(s) {
            let log = gl.get_shader_info_log(s);
            gl.delete_shader(s);
            gl.delete_program(program);
            return Err(format!("shader compilation failed: {log}"));
        }
        gl.attach_shader(program, s);
        shaders.push(s);
    }
    gl.bind_attrib_location(program, 0, "a_pos");
    gl.link_program(program);
    for s in shaders {
        gl.detach_shader(program, s);
        gl.delete_shader(s);
    }
    if !gl.get_program_link_status(program) {
        let log = gl.get_program_info_log(program);
        gl.delete_program(program);
        return Err(format!("shader link failed: {log}"));
    }
    gl.use_program(Some(program));
    for (i, n) in samplers.iter().enumerate() {
        gl.uniform_1_i32(gl.get_uniform_location(program, n).as_ref(), i as i32);
    }
    let p = Program { u_csc: gl.get_uniform_location(program, "u_csc"), u_off: gl.get_uniform_location(program, "u_off"), u_scale: gl.get_uniform_location(program, "u_scale"), program, csc: None };
    gl.uniform_2_f32(p.u_scale.as_ref(), 1.0, 1.0);
    Ok(p)
}

unsafe fn new_texture(gl: &glow::Context) -> Result<glow::Texture, String> {
    let t = gl.create_texture()?;
    gl.bind_texture(glow::TEXTURE_2D, Some(t));
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
    gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
    Ok(t)
}

impl Renderer {
    unsafe fn new(gl: &glow::Context) -> Result<Self, String> {
        let sv = egui_glow::ShaderVersion::get(gl);
        let modern = sv.is_new_shader_interface();
        let head = format!("{}#ifdef GL_ES\nprecision mediump float;\n#endif\n", sv.version_declaration());
        let (attr, vout, fin) = if modern { ("in", "out", "in") } else { ("attribute", "varying", "varying") };
        let vs = format!(
            "{head}{attr} vec2 a_pos;\n{vout} vec2 v_uv;\nvoid main() {{\n  v_uv = vec2(a_pos.x * 0.5 + 0.5, 0.5 - a_pos.y * 0.5);\n  gl_Position = vec4(a_pos, 0.0, 1.0);\n}}\n"
        );
        let (tex, out_decl, out_var) = if modern { ("texture", "out vec4 frag_color;\n", "frag_color") } else { ("texture2D", "", "gl_FragColor") };
        let common = format!("{head}uniform mat3 u_csc;\nuniform vec3 u_off;\nuniform vec2 u_scale;\n{fin} vec2 v_uv;\n{out_decl}");
        let fs_i420 = format!(
            "{common}uniform sampler2D u_y;\nuniform sampler2D u_u;\nuniform sampler2D u_v;\nvoid main() {{\n  vec2 t = v_uv * u_scale;\n  vec3 yuv = vec3({tex}(u_y, t).r, {tex}(u_u, t).r, {tex}(u_v, t).r) - u_off;\n  {out_var} = vec4(clamp(u_csc * yuv, 0.0, 1.0), 1.0);\n}}\n"
        );
        let fs_nv12 = format!(
            "{common}uniform sampler2D u_y;\nuniform sampler2D u_uv;\nvoid main() {{\n  vec2 t = v_uv * u_scale;\n  vec3 yuv = vec3({tex}(u_y, t).r, {tex}(u_uv, t).rg) - u_off;\n  {out_var} = vec4(clamp(u_csc * yuv, 0.0, 1.0), 1.0);\n}}\n"
        );
        // HDR10 (BT.2020, PQ) -> SDR: same maths as moon_media::hdr::hdr10_to_sdr.
        let fs_hdr = format!(
            "{common}uniform sampler2D u_y;\nuniform sampler2D u_u;\nuniform sampler2D u_v;\nuniform float u_peak;\nuniform float u_scale10;\n\
             float pq(float e) {{ float p = pow(clamp(e, 0.0, 1.0), 1.0 / 78.84375); return pow(max(p - 0.8359375, 0.0) / (18.8515625 - 18.6875 * p), 1.0 / 0.1593017578125); }}\n\
             float tone(float x) {{ if (u_peak <= 1.0 || x <= 0.75) return min(x, 1.0); return 1.0 - 0.25 * exp(-(x - 0.75) / 0.25); }}\n\
             float srgb(float c) {{ return c <= 0.0031308 ? c * 12.92 : 1.055 * pow(c, 1.0 / 2.4) - 0.055; }}\n\
             void main() {{\n  vec2 t = v_uv * u_scale;\n\
               float y = ({tex}(u_y, t).r * u_scale10 - 64.0) / 876.0;\n\
               float cb = ({tex}(u_u, t).r * u_scale10 - 512.0) / 896.0;\n\
               float cr = ({tex}(u_v, t).r * u_scale10 - 512.0) / 896.0;\n\
               float r = y + 1.4746 * cr; float b = y + 1.8814 * cb; float g = (y - 0.2627 * r - 0.0593 * b) / 0.6780;\n\
               vec3 l = vec3(pq(r), pq(g), pq(b)) * (10000.0 / 203.0);\n\
               vec3 s = vec3(dot(vec3(1.660491, -0.587641, -0.072850), l), dot(vec3(-0.124550, 1.132900, -0.008349), l), dot(vec3(-0.018151, -0.100579, 1.118730), l));\n\
               {out_var} = vec4(srgb(tone(max(s.r, 0.0))), srgb(tone(max(s.g, 0.0))), srgb(tone(max(s.b, 0.0))), 1.0);\n}}\n"
        );
        let i420 = compile(gl, &vs, &fs_i420, &["u_y", "u_u", "u_v"])?;
        let hdr = if modern { compile(gl, &vs, &fs_hdr, &["u_y", "u_u", "u_v"]).map_err(|e| warn!("HDR shader: {e}")).ok() } else { None };
        let (u_peak, u_scale10) = match &hdr {
            Some(p) => (gl.get_uniform_location(p.program, "u_peak"), gl.get_uniform_location(p.program, "u_scale10")),
            None => (None, None),
        };
        let norm16 = !gl.version().is_embedded;
        // NV12 needs two-channel textures (GL3 / GLES3).
        let nv12 = if modern { compile(gl, &vs, &fs_nv12, &["u_y", "u_uv"]).ok() } else { None };

        // Full-viewport quad (egui sets the viewport to the callback rect).
        let quad: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
        let bytes: Vec<u8> = quad.iter().flat_map(|v| v.to_ne_bytes()).collect();
        let vbo = gl.create_buffer()?;
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STATIC_DRAW);
        let vao = if modern { gl.create_vertex_array().ok() } else { None };
        if let Some(v) = vao {
            gl.bind_vertex_array(Some(v));
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
            gl.bind_vertex_array(None);
        }
        gl.bind_buffer(glow::ARRAY_BUFFER, None);
        let tex = [new_texture(gl)?, new_texture(gl)?, new_texture(gl)?];
        let tex_nv12 = [new_texture(gl)?, new_texture(gl)?];
        let tex_cuda = [new_texture(gl)?, new_texture(gl)?];
        let tex16 = [new_texture(gl)?, new_texture(gl)?, new_texture(gl)?];
        gl.bind_texture(glow::TEXTURE_2D, None);
        Ok(Self { i420, nv12, hdr, u_peak, u_scale10, tex16, size16: [0, 0], norm16, vbo, vao, a_pos: 0, tex, tex_nv12, tex_cuda, cuda: None, size: [0, 0], yuv444: false, csc: Csc::default(), source: Source::None, imported: None, modern })
    }

    unsafe fn upload(&mut self, gl: &glow::Context, f: &I420Frame) {
        let (w, h) = (f.width, f.height);
        let (cw, ch) = (f.chroma_width(), f.chroma_height());
        let realloc = self.size != [w, h] || self.yuv444 != f.yuv444 || !matches!(self.source, Source::I420);
        self.yuv444 = f.yuv444;
        let (ifmt, fmt) = if self.modern { (glow::R8 as i32, glow::RED) } else { (glow::LUMINANCE as i32, glow::LUMINANCE) };
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        for (t, (data, pw, ph)) in self.tex.iter().zip([(&f.y, w, h), (&f.u, cw, ch), (&f.v, cw, ch)]) {
            gl.bind_texture(glow::TEXTURE_2D, Some(*t));
            let px = glow::PixelUnpackData::Slice(Some(&data[..pw * ph]));
            if realloc {
                gl.tex_image_2d(glow::TEXTURE_2D, 0, ifmt, pw as i32, ph as i32, 0, fmt, glow::UNSIGNED_BYTE, px);
            } else {
                gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, pw as i32, ph as i32, fmt, glow::UNSIGNED_BYTE, px);
            }
        }
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
        gl.bind_texture(glow::TEXTURE_2D, None);
        self.size = [w, h];
        self.csc = f.csc;
        self.source = Source::I420;
        self.release_imported();
    }

    /// Upload a 10-bit HDR10 picture (16-bit textures; 8-bit on GLES).
    unsafe fn upload16(&mut self, gl: &glow::Context, f: &Yuv16Frame, peak: f32) {
        if self.hdr.is_none() {
            return;
        }
        let (w, h) = (f.width, f.height);
        let (cw, ch) = (f.chroma_width(), f.chroma_height());
        let realloc = self.size16 != [w, h];
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, if self.norm16 { 2 } else { 1 });
        for (t, (data, pw, ph)) in self.tex16.iter().zip([(&f.y, w, h), (&f.u, cw, ch), (&f.v, cw, ch)]) {
            gl.bind_texture(glow::TEXTURE_2D, Some(*t));
            let plane = &data[..pw * ph];
            if self.norm16 {
                let bytes = std::slice::from_raw_parts(plane.as_ptr() as *const u8, plane.len() * 2);
                let px = glow::PixelUnpackData::Slice(Some(bytes));
                if realloc {
                    gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::R16 as i32, pw as i32, ph as i32, 0, glow::RED, glow::UNSIGNED_SHORT, px);
                } else {
                    gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, pw as i32, ph as i32, glow::RED, glow::UNSIGNED_SHORT, px);
                }
            } else {
                let bytes: Vec<u8> = plane.iter().map(|v| (v >> 8) as u8).collect();
                let px = glow::PixelUnpackData::Slice(Some(&bytes));
                if realloc {
                    gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::R8 as i32, pw as i32, ph as i32, 0, glow::RED, glow::UNSIGNED_BYTE, px);
                } else {
                    gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, pw as i32, ph as i32, glow::RED, glow::UNSIGNED_BYTE, px);
                }
            }
        }
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
        gl.bind_texture(glow::TEXTURE_2D, None);
        self.size16 = [w, h];
        self.size = [w, h];
        self.source = Source::Hdr { peak };
        self.release_imported();
    }

    /// Bind a VAAPI surface to the NV12 textures through DMA-BUF / EGL images.
    unsafe fn import_vaapi(&mut self, gl: &glow::Context, v: VaapiFrame) -> Result<(), String> {
        if self.nv12.is_none() {
            return Err("OpenGL 3 / GLES 3 is needed".into());
        }
        let egl = egl()?;
        let display = (egl.get_current_display)();
        if display.is_null() {
            return Err("the window does not use EGL".into());
        }
        let exts = {
            let p = (egl.query_string)(display, EGL_EXTENSIONS);
            if p.is_null() { String::new() } else { std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned() }
        };
        if !exts.contains("EGL_EXT_image_dma_buf_import") {
            return Err("EGL cannot import DMA-BUF".into());
        }
        let modifiers = exts.contains("EGL_EXT_image_dma_buf_import_modifiers");
        let s = moon_media::vaapi::export_surface(v.display, v.surface).map_err(|e| format!("{e:#}"))?;
        if s.planes.len() != 2 {
            return Err(format!("expected an NV12 surface (2 planes), got {} planes", s.planes.len()));
        }
        let mut images = Vec::new();
        for (i, p) in s.planes.iter().enumerate() {
            let (w, h) = if i == 0 { (s.width, s.height) } else { (s.width.div_ceil(2), s.height.div_ceil(2)) };
            let mut attrs = vec![
                EGL_WIDTH, w as i32, EGL_HEIGHT, h as i32,
                EGL_LINUX_DRM_FOURCC_EXT, p.drm_format as i32,
                EGL_DMA_BUF_PLANE0_FD_EXT, p.fd,
                EGL_DMA_BUF_PLANE0_OFFSET_EXT, p.offset as i32,
                EGL_DMA_BUF_PLANE0_PITCH_EXT, p.pitch as i32,
            ];
            if modifiers && p.modifier != DRM_FORMAT_MOD_INVALID {
                attrs.extend_from_slice(&[EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, p.modifier as u32 as i32, EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (p.modifier >> 32) as u32 as i32]);
            }
            attrs.push(EGL_NONE);
            let img = (egl.create_image)(display, std::ptr::null_mut(), EGL_LINUX_DMA_BUF_EXT, std::ptr::null_mut(), attrs.as_ptr());
            if img.is_null() {
                for im in images {
                    (egl.destroy_image)(display, im);
                }
                return Err(format!("eglCreateImageKHR failed for plane {i} (format 0x{:08x})", p.drm_format));
            }
            gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_nv12[i]));
            (egl.image_target_texture)(glow::TEXTURE_2D, img);
            images.push(img);
        }
        gl.bind_texture(glow::TEXTURE_2D, None);
        let (surface_w, surface_h) = (s.width.max(1) as f32, s.height.max(1) as f32);
        drop(s); // the EGL images keep the buffers; the fds can be closed
        if self.imported.is_none() && !matches!(self.source, Source::Nv12 { .. }) {
            info!("zero-copy display: VAAPI surfaces imported through DMA-BUF");
        }
        self.release_imported();
        self.size = [v.width, v.height];
        self.csc = v.csc;
        // The surface may be padded (e.g. 1088 lines for 1080): sample only the picture.
        self.source = Source::Nv12 { scale: [(v.width as f32 / surface_w).min(1.0), (v.height as f32 / surface_h).min(1.0)] };
        self.imported = Some(Imported { _frame: v, display, images });
        Ok(())
    }

    /// Copy an NVDEC picture straight from video memory into the CUDA textures.
    unsafe fn import_cuda(&mut self, gl: &glow::Context, c: &CudaFrame) -> Result<(), String> {
        if self.nv12.is_none() {
            return Err("OpenGL 3 / GLES 3 is needed".into());
        }
        if !self.cuda.as_ref().is_some_and(|t| t.fits(c)) {
            self.cuda = None;
            let (w, h) = (c.width as i32, c.height as i32);
            for (t, (ifmt, fmt, tw, th)) in self.tex_cuda.iter().zip([(glow::R8, glow::RED, w, h), (glow::RG8, glow::RG, (w + 1) / 2, (h + 1) / 2)]) {
                gl.bind_texture(glow::TEXTURE_2D, Some(*t));
                gl.tex_image_2d(glow::TEXTURE_2D, 0, ifmt as i32, tw, th, 0, fmt, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
            }
            gl.bind_texture(glow::TEXTURE_2D, None);
            let target = CudaGlTarget::register(c, self.tex_cuda[0].0.get(), self.tex_cuda[1].0.get()).map_err(|e| format!("{e:#}"))?;
            info!("zero-copy display: NVDEC pictures copied by CUDA into OpenGL textures");
            self.cuda = Some(target);
        }
        self.cuda.as_mut().unwrap().copy(c).map_err(|e| format!("{e:#}"))?;
        self.release_imported();
        self.size = [c.width, c.height];
        self.csc = c.csc;
        self.source = Source::Cuda;
        Ok(())
    }

    fn release_imported(&mut self) {
        if let Some(im) = self.imported.take() {
            if let Ok(egl) = egl() {
                for i in im.images {
                    unsafe { (egl.destroy_image)(im.display, i) };
                }
            }
        }
    }

    unsafe fn draw(&mut self, gl: &glow::Context) {
        let (prog, textures, scale): (&mut Program, Vec<glow::Texture>, [f32; 2]) = match self.source {
            Source::None => return,
            Source::I420 => (&mut self.i420, self.tex.to_vec(), [1.0, 1.0]),
            Source::Nv12 { scale } => match self.nv12.as_mut() {
                Some(p) => (p, self.tex_nv12.to_vec(), scale),
                None => return,
            },
            Source::Cuda => match self.nv12.as_mut() {
                Some(p) => (p, self.tex_cuda.to_vec(), [1.0, 1.0]),
                None => return,
            },
            Source::Hdr { peak } => match self.hdr.as_mut() {
                Some(p) => {
                    gl.use_program(Some(p.program));
                    gl.uniform_1_f32(self.u_peak.as_ref(), peak);
                    // texel -> 10-bit code value: 16-bit MSB-aligned, or 8-bit on GLES
                    gl.uniform_1_f32(self.u_scale10.as_ref(), if self.norm16 { 65535.0 / 64.0 } else { 255.0 * 4.0 });
                    (p, self.tex16.to_vec(), [1.0, 1.0])
                }
                None => return,
            },
        };
        gl.disable(glow::BLEND);
        gl.use_program(Some(prog.program));
        if prog.csc != Some(self.csc) {
            let (m, off) = self.csc.rgb_matrix();
            gl.uniform_matrix_3_f32_slice(prog.u_csc.as_ref(), false, &m);
            gl.uniform_3_f32(prog.u_off.as_ref(), off[0], off[1], off[2]);
            prog.csc = Some(self.csc);
        }
        gl.uniform_2_f32(prog.u_scale.as_ref(), scale[0], scale[1]);
        for (i, t) in textures.iter().enumerate() {
            gl.active_texture(glow::TEXTURE0 + i as u32);
            gl.bind_texture(glow::TEXTURE_2D, Some(*t));
        }
        match self.vao {
            Some(v) => gl.bind_vertex_array(Some(v)),
            None => {
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
                gl.enable_vertex_attrib_array(self.a_pos);
                gl.vertex_attrib_pointer_f32(self.a_pos, 2, glow::FLOAT, false, 8, 0);
            }
        }
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        match self.vao {
            Some(_) => gl.bind_vertex_array(None),
            None => {
                gl.disable_vertex_attrib_array(self.a_pos);
                gl.bind_buffer(glow::ARRAY_BUFFER, None);
            }
        }
        for i in (0..textures.len()).rev() {
            gl.active_texture(glow::TEXTURE0 + i as u32);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    unsafe fn destroy(mut self, gl: &glow::Context) {
        self.release_imported();
        self.cuda = None;
        gl.delete_program(self.i420.program);
        for p in self.nv12.iter().chain(self.hdr.iter()) {
            gl.delete_program(p.program);
        }
        gl.delete_buffer(self.vbo);
        if let Some(v) = self.vao {
            gl.delete_vertex_array(v);
        }
        for t in self.tex.into_iter().chain(self.tex_nv12).chain(self.tex_cuda).chain(self.tex16) {
            gl.delete_texture(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{c_int, c_uint};

    /// Headless OpenGL context on a GPU (EGL device platform); None if unavailable.
    fn headless_gl() -> Option<(libloading::Library, glow::Context)> {
        unsafe {
            let lib = libloading::Library::new("libEGL.so.1").ok()?;
            let gp: unsafe extern "C" fn(*const c_char) -> *mut c_void = *lib.get(b"eglGetProcAddress\0").ok()?;
            let f = |n: &[u8]| gp(n.as_ptr() as *const c_char);
            let query_devices: unsafe extern "C" fn(c_int, *mut *mut c_void, *mut c_int) -> c_uint = std::mem::transmute(f(b"eglQueryDevicesEXT\0").as_ref()?);
            let platform_display: unsafe extern "C" fn(c_uint, *mut c_void, *const c_int) -> *mut c_void = std::mem::transmute(f(b"eglGetPlatformDisplayEXT\0").as_ref()?);
            let initialize: unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_uint = *lib.get(b"eglInitialize\0").ok()?;
            let bind_api: unsafe extern "C" fn(c_uint) -> c_uint = *lib.get(b"eglBindAPI\0").ok()?;
            let choose: unsafe extern "C" fn(*mut c_void, *const c_int, *mut *mut c_void, c_int, *mut c_int) -> c_uint = *lib.get(b"eglChooseConfig\0").ok()?;
            let create_ctx: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *const c_int) -> *mut c_void = *lib.get(b"eglCreateContext\0").ok()?;
            let make_current: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> c_uint = *lib.get(b"eglMakeCurrent\0").ok()?;
            let mut devs = [std::ptr::null_mut(); 8];
            let mut n = 0;
            if query_devices(8, devs.as_mut_ptr(), &mut n) == 0 {
                return None;
            }
            for dev in &devs[..n as usize] {
                let dpy = platform_display(0x313F, *dev, [0x3038].as_ptr());
                let (mut ma, mut mi) = (0, 0);
                if dpy.is_null() || initialize(dpy, &mut ma, &mut mi) == 0 || bind_api(0x30A2) == 0 {
                    continue;
                }
                let attrs = [0x3033, 0x0001, 0x3040, 0x0008, 0x3038];
                let (mut cfg, mut num) = (std::ptr::null_mut(), 0);
                if choose(dpy, attrs.as_ptr(), &mut cfg, 1, &mut num) == 0 || num == 0 {
                    continue;
                }
                let ctx = create_ctx(dpy, cfg, std::ptr::null_mut(), [0x3038].as_ptr());
                if !ctx.is_null() && make_current(dpy, std::ptr::null_mut(), std::ptr::null_mut(), ctx) != 0 {
                    let gl = glow::Context::from_loader_function_cstr(|s| gp(s.as_ptr()) as *const c_void);
                    return Some((lib, gl));
                }
            }
            None
        }
    }

    /// Draw with `draw` into a w x h RGBA framebuffer and read it back.
    unsafe fn render(gl: &glow::Context, w: usize, h: usize, draw: impl FnOnce()) -> Vec<[u8; 3]> {
        let tex = gl.create_texture().unwrap();
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        gl.tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA8 as i32, w as i32, h as i32, 0, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(None));
        let fb = gl.create_framebuffer().unwrap();
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fb));
        gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
        gl.viewport(0, 0, w as i32, h as i32);
        draw();
        let mut px = vec![0u8; w * h * 4];
        gl.read_pixels(0, 0, w as i32, h as i32, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut px)));
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        // GL rows start at the bottom; the shader flips v so row 0 is the top of the picture.
        let mut rows: Vec<[u8; 3]> = Vec::with_capacity(w * h);
        for y in (0..h).rev() {
            for x in 0..w {
                let o = (y * w + x) * 4;
                rows.push([px[o], px[o + 1], px[o + 2]]);
            }
        }
        rows
    }

    /// The real shaders, run on the GPU, agree with the CPU reference conversions.
    #[test]
    fn shaders_match_cpu_conversion() {
        let Some((_egl, gl)) = headless_gl() else {
            eprintln!("no headless OpenGL context, skipping");
            return;
        };
        let (w, h) = (64usize, 16usize);
        unsafe {
            let mut r = Renderer::new(&gl).expect("shaders must compile");
            // SDR I420: horizontal gradient + colour bands.
            let mut f = I420Frame::new(w, h);
            for (i, p) in f.y.iter_mut().enumerate() {
                *p = (16 + (i % w) * 219 / (w - 1)) as u8;
            }
            for (i, (u, v)) in f.u.iter_mut().zip(f.v.iter_mut()).enumerate() {
                (*u, *v) = ((40 + (i % (w / 2)) * 5) as u8, (200 - (i % (w / 2)) * 4) as u8);
            }
            r.upload(&gl, &f);
            let got = render(&gl, w, h, || r.draw(&gl));
            let mut cpu = vec![0u32; w * h];
            f.to_xrgb(&mut cpu);
            let mut max_err = 0;
            for (g, c) in got.iter().zip(&cpu) {
                let c = [(c >> 16) as u8, (c >> 8) as u8, *c as u8];
                for k in 0..3 {
                    max_err = max_err.max((g[k] as i32 - c[k] as i32).abs());
                }
            }
            eprintln!("I420 shader vs CPU: max error {max_err}");
            assert!(max_err <= 3, "I420 shader differs from the CPU conversion ({max_err})");

            // HDR10 -> SDR.
            if r.hdr.is_some() {
                let bars: [[u8; 3]; 8] = [[255, 255, 255], [20, 20, 20], [230, 40, 40], [40, 200, 60], [50, 60, 220], [128, 128, 128], [240, 200, 40], [40, 200, 220]];
                let mut src = vec![0u8; w * h * 4];
                for (i, px) in src.chunks_exact_mut(4).enumerate() {
                    let c = bars[(i % w) * 8 / w];
                    px.copy_from_slice(&[c[2], c[1], c[0], 255]);
                }
                let mut p = moon_media::hdr::P010Frame::new(w, h);
                p.from_rgb32_scaled(&src, w, h, w * 4, false, w, h);
                let mut f16 = Yuv16Frame::new(1, 1);
                f16.copy_from_p010(w, h, &p.y, w, &p.uv, w);
                r.upload16(&gl, &f16, 1.0);
                let got = render(&gl, w, h, || r.draw(&gl));
                let mut cpu = vec![0u32; w * h];
                f16.to_xrgb_hdr(&mut cpu, 1.0);
                // Away from the bar edges: there the GPU interpolates chroma between two
                // saturated colours (bilinear filtering) while the CPU reference takes the
                // nearest sample.
                let mut max_err = 0;
                for (i, (g, c)) in got.iter().zip(&cpu).enumerate() {
                    let x = i % w;
                    let edge = (x * 8) % w;
                    if edge < 3 * 8 || edge > w - 3 * 8 {
                        continue;
                    }
                    let c = [(c >> 16) as u8, (c >> 8) as u8, *c as u8];
                    for k in 0..3 {
                        max_err = max_err.max((g[k] as i32 - c[k] as i32).abs());
                    }
                }
                eprintln!("HDR shader vs CPU: max error {max_err}");
                assert!(max_err <= 4, "HDR shader differs from the CPU conversion ({max_err})");
            }
        }
    }
}

#[cfg(test)]
mod pacing_tests {
    use super::*;

    /// Two frames arriving together (a Wi-Fi burst) are both shown, one interval apart.
    #[test]
    fn burst_frames_are_paced_not_dropped() {
        let v = VideoView::default();
        let g = v.reset(60, true);
        let f = I420Frame::new(16, 16);
        v.put(g, Decoded::Cpu(&f), 1);
        v.put(g, Decoded::Cpu(&f), 2);
        assert!(v.take_due().is_some(), "first frame shown at once");
        assert!(v.take_due().is_none(), "second frame waits for its turn");
        let wait = v.next_due().expect("a frame is waiting");
        assert!(wait > Duration::from_millis(10) && wait <= Duration::from_millis(15), "{wait:?}");
        std::thread::sleep(wait);
        assert!(v.take_due().is_some(), "second frame shown one interval later");
        assert_eq!(v.replaced(), 0);
        // A real backlog stays bounded.
        for _ in 0..5 {
            v.put(g, Decoded::Cpu(&f), 0);
        }
        assert_eq!(v.replaced(), 3);
    }

    /// Without pacing, a burst keeps only the newest frame and shows it at once.
    #[test]
    fn minimum_latency_shows_newest_at_once() {
        let v = VideoView::default();
        let g = v.reset(60, false);
        let f = I420Frame::new(16, 16);
        v.put(g, Decoded::Cpu(&f), 1);
        assert!(v.take_due().is_some());
        v.put(g, Decoded::Cpu(&f), 2);
        v.put(g, Decoded::Cpu(&f), 3);
        assert_eq!(v.replaced(), 1, "frame 2 replaced by 3");
        assert_eq!(v.next_due(), Some(Duration::ZERO));
        assert!(v.take_due().is_some(), "shown without waiting an interval");
        assert_eq!(v.take_shown().last().map(|s| s.index), Some(3));
    }
}
