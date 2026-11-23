//! GPU capture-to-encoder path: the compositor's DMA-BUF is imported with EGL and a
//! shader converts it to YUV (with scaling). NVIDIA: copied into NVENC's CUDA buffer;
//! Intel / AMD: drawn into the VAAPI encoder's surface. No pixel touches the CPU.
//! Headless EGL; everything stays on the thread that created the converter.

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};

use anyhow::{anyhow, bail, Context as _, Result};
use glow::HasContext;
use libloading::Library;
use tracing::{debug, info};

use crate::frame::Csc;
use crate::nvenc::CudaInput;

/// DRM fourcc codes of the 32-bit RGB layouts compositors hand out.
pub const DRM_FORMAT_XRGB8888: u32 = fourcc(b"XR24");
pub const DRM_FORMAT_ARGB8888: u32 = fourcc(b"AR24");
pub const DRM_FORMAT_XBGR8888: u32 = fourcc(b"XB24");
pub const DRM_FORMAT_ABGR8888: u32 = fourcc(b"AB24");
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

const fn fourcc(c: &[u8; 4]) -> u32 {
    c[0] as u32 | (c[1] as u32) << 8 | (c[2] as u32) << 16 | (c[3] as u32) << 24
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmabufPlane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
}

/// One captured picture in a DMA-BUF (borrowed: the capture keeps the buffer
/// alive and untouched while the converter reads it).
#[derive(Clone, Debug)]
pub struct DmabufFrame {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
    /// Stable identity of the underlying buffer (the compositor recycles a few).
    pub key: usize,
}

// ---- EGL ---------------------------------------------------------------------

type EglDisplay = *mut c_void;
type EglContext = *mut c_void;
type EglImage = *mut c_void;
type EglDevice = *mut c_void;

const EGL_NONE: i32 = 0x3038;
const EGL_EXTENSIONS: i32 = 0x3055;
const EGL_VENDOR: i32 = 0x3053;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_OPENGL_API: u32 = 0x30A2;
const EGL_CONTEXT_MAJOR_VERSION: i32 = 0x3098;
const EGL_CONTEXT_MINOR_VERSION: i32 = 0x30FB;
const EGL_CONTEXT_OPENGL_PROFILE_MASK: i32 = 0x30FD;
const EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT: i32 = 1;
const EGL_PLATFORM_DEVICE_EXT: u32 = 0x313F;
const EGL_DRM_DEVICE_FILE_EXT: i32 = 0x3233;
const EGL_DRM_RENDER_NODE_FILE_EXT: i32 = 0x3377;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_IMAGE_PRESERVED_KHR: i32 = 0x30D2;
/// Per plane: fd, offset, pitch, modifier lo, modifier hi.
const EGL_PLANE_ATTRS: [[i32; 5]; 4] = [
    [0x3272, 0x3273, 0x3274, 0x3443, 0x3444],
    [0x3275, 0x3276, 0x3277, 0x3445, 0x3446],
    [0x3278, 0x3279, 0x327A, 0x3447, 0x3448],
    [0x3440, 0x3441, 0x3442, 0x3449, 0x344A],
];

struct Egl {
    _lib: Library,
    get_proc: unsafe extern "C" fn(*const c_char) -> *const c_void,
    initialize: unsafe extern "C" fn(EglDisplay, *mut i32, *mut i32) -> u32,
    terminate: unsafe extern "C" fn(EglDisplay) -> u32,
    bind_api: unsafe extern "C" fn(u32) -> u32,
    create_context: unsafe extern "C" fn(EglDisplay, *mut c_void, EglContext, *const i32) -> EglContext,
    destroy_context: unsafe extern "C" fn(EglDisplay, EglContext) -> u32,
    make_current: unsafe extern "C" fn(EglDisplay, *mut c_void, *mut c_void, EglContext) -> u32,
    query_string: unsafe extern "C" fn(EglDisplay, i32) -> *const c_char,
    get_error: unsafe extern "C" fn() -> i32,
    create_image: unsafe extern "C" fn(EglDisplay, EglContext, u32, *mut c_void, *const i32) -> EglImage,
    destroy_image: unsafe extern "C" fn(EglDisplay, EglImage) -> u32,
    query_modifiers: unsafe extern "C" fn(EglDisplay, i32, i32, *mut u64, *mut u32, *mut i32) -> u32,
    image_target_texture: unsafe extern "C" fn(u32, EglImage),
}

impl Egl {
    fn load() -> Result<Self> {
        unsafe {
            let lib = Library::new("libEGL.so.1").context("cannot load libEGL.so.1")?;
            macro_rules! sym {
                ($n:literal) => {
                    *lib.get(concat!($n, "\0").as_bytes()).with_context(|| format!("EGL symbol {}", $n))?
                };
            }
            let get_proc: unsafe extern "C" fn(*const c_char) -> *const c_void = sym!("eglGetProcAddress");
            let ext = |name: &str| -> Result<*const c_void> {
                let c = CString::new(name).unwrap();
                let p = get_proc(c.as_ptr());
                if p.is_null() {
                    bail!("EGL/GL function {name} is not available");
                }
                Ok(p)
            };
            Ok(Self {
                get_proc,
                initialize: sym!("eglInitialize"),
                terminate: sym!("eglTerminate"),
                bind_api: sym!("eglBindAPI"),
                create_context: sym!("eglCreateContext"),
                destroy_context: sym!("eglDestroyContext"),
                make_current: sym!("eglMakeCurrent"),
                query_string: sym!("eglQueryString"),
                get_error: sym!("eglGetError"),
                create_image: std::mem::transmute::<*const c_void, unsafe extern "C" fn(EglDisplay, EglContext, u32, *mut c_void, *const i32) -> EglImage>(ext("eglCreateImageKHR")?),
                destroy_image: std::mem::transmute::<*const c_void, unsafe extern "C" fn(EglDisplay, EglImage) -> u32>(ext("eglDestroyImageKHR")?),
                query_modifiers: std::mem::transmute::<*const c_void, unsafe extern "C" fn(EglDisplay, i32, i32, *mut u64, *mut u32, *mut i32) -> u32>(ext("eglQueryDmaBufModifiersEXT")?),
                image_target_texture: std::mem::transmute::<*const c_void, unsafe extern "C" fn(u32, EglImage)>(ext("glEGLImageTargetTexture2DOES")?),
                _lib: lib,
            })
        }
    }

    fn proc(&self, name: &str) -> *const c_void {
        let c = CString::new(name).unwrap();
        unsafe { (self.get_proc)(c.as_ptr()) }
    }

    fn string(&self, dpy: EglDisplay, what: i32) -> String {
        let p = unsafe { (self.query_string)(dpy, what) };
        if p.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
        }
    }

    /// The EGL display of the GPU whose render node is `prefer` (the VAAPI
    /// encoder's), else of the first GPU that has a DRM node (the NVIDIA card).
    fn device_display(&self, prefer: Option<&str>) -> Result<EglDisplay> {
        type QueryDevices = unsafe extern "C" fn(i32, *mut EglDevice, *mut i32) -> u32;
        type QueryDeviceString = unsafe extern "C" fn(EglDevice, i32) -> *const c_char;
        type GetPlatformDisplay = unsafe extern "C" fn(u32, *mut c_void, *const i32) -> EglDisplay;
        let (qd, qds, gpd) = (self.proc("eglQueryDevicesEXT"), self.proc("eglQueryDeviceStringEXT"), self.proc("eglGetPlatformDisplayEXT"));
        if qd.is_null() || qds.is_null() || gpd.is_null() {
            bail!("EGL has no device platform (EGL_EXT_device_enumeration / EGL_EXT_platform_device)");
        }
        let (qd, qds, gpd) = unsafe { (std::mem::transmute::<*const c_void, QueryDevices>(qd), std::mem::transmute::<*const c_void, QueryDeviceString>(qds), std::mem::transmute::<*const c_void, GetPlatformDisplay>(gpd)) };
        let mut devs = [std::ptr::null_mut(); 16];
        let mut n = 0;
        if unsafe { qd(devs.len() as i32, devs.as_mut_ptr(), &mut n) } == 0 || n <= 0 {
            bail!("no EGL device");
        }
        let node_of = |d: EglDevice| -> String {
            let p = unsafe { qds(d, EGL_DRM_RENDER_NODE_FILE_EXT) };
            if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
        };
        let mut order: Vec<EglDevice> = devs[..n as usize].to_vec();
        if let Some(want) = prefer {
            order.sort_by_key(|d| node_of(*d) != want);
        }
        for d in order {
            let exts = unsafe { qds(d, EGL_EXTENSIONS) };
            let has_drm = !exts.is_null() && unsafe { CStr::from_ptr(exts) }.to_string_lossy().contains("EGL_EXT_device_drm");
            if !has_drm {
                continue;
            }
            let file = unsafe { qds(d, EGL_DRM_DEVICE_FILE_EXT) };
            let file = if file.is_null() { String::new() } else { unsafe { CStr::from_ptr(file) }.to_string_lossy().into_owned() };
            let dpy = unsafe { gpd(EGL_PLATFORM_DEVICE_EXT, d, std::ptr::null()) };
            if !dpy.is_null() {
                debug!("EGL device {file} ({})", node_of(d));
                return Ok(dpy);
            }
        }
        bail!("no EGL device with a DRM node")
    }
}

// ---- CUDA (driver API, loaded at runtime) --------------------------------------

type CuResource = *mut c_void;
type CuArray = *mut c_void;

#[repr(C)]
struct CudaMemcpy2D {
    src_x_in_bytes: usize,
    src_y: usize,
    src_memory_type: u32,
    src_host: *const c_void,
    src_device: u64,
    src_array: CuArray,
    src_pitch: usize,
    dst_x_in_bytes: usize,
    dst_y: usize,
    dst_memory_type: u32,
    dst_host: *mut c_void,
    dst_device: u64,
    dst_array: CuArray,
    dst_pitch: usize,
    width_in_bytes: usize,
    height: usize,
}

const CU_MEMORYTYPE_DEVICE: u32 = 2;
const CU_MEMORYTYPE_ARRAY: u32 = 3;
const CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY: u32 = 1;

struct Cuda {
    _lib: Library,
    push: unsafe extern "C" fn(*mut c_void) -> i32,
    pop: unsafe extern "C" fn(*mut *mut c_void) -> i32,
    register_image: unsafe extern "C" fn(*mut CuResource, u32, u32, u32) -> i32,
    unregister: unsafe extern "C" fn(CuResource) -> i32,
    map: unsafe extern "C" fn(u32, *mut CuResource, *mut c_void) -> i32,
    unmap: unsafe extern "C" fn(u32, *mut CuResource, *mut c_void) -> i32,
    mapped_array: unsafe extern "C" fn(*mut CuArray, CuResource, u32, u32) -> i32,
    memcpy_2d: unsafe extern "C" fn(*const CudaMemcpy2D) -> i32,
}

impl Cuda {
    fn load() -> Result<Self> {
        unsafe {
            let lib = Library::new("libcuda.so.1").context("cannot load libcuda.so.1")?;
            macro_rules! sym {
                ($n:literal) => {
                    *lib.get(concat!($n, "\0").as_bytes()).with_context(|| format!("CUDA symbol {}", $n))?
                };
            }
            Ok(Self {
                push: sym!("cuCtxPushCurrent_v2"),
                pop: sym!("cuCtxPopCurrent_v2"),
                register_image: sym!("cuGraphicsGLRegisterImage"),
                unregister: sym!("cuGraphicsUnregisterResource"),
                map: sym!("cuGraphicsMapResources"),
                unmap: sym!("cuGraphicsUnmapResources"),
                mapped_array: sym!("cuGraphicsSubResourceGetMappedArray"),
                memcpy_2d: sym!("cuMemcpy2D_v2"),
                _lib: lib,
            })
        }
    }

    /// Run `f` with `ctx` current on this thread.
    fn with<R>(&self, ctx: *mut c_void, f: impl FnOnce() -> R) -> R {
        unsafe { (self.push)(ctx) };
        let r = f();
        let mut popped = std::ptr::null_mut();
        unsafe { (self.pop)(&mut popped) };
        r
    }
}

fn cu(r: i32, what: &str) -> Result<()> {
    if r != 0 {
        bail!("{what} failed (CUDA error {r})");
    }
    Ok(())
}

// ---- Shaders -------------------------------------------------------------------

const VERTEX: &str = r#"#version 330 core
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

/// One byte per texel, laid out like NVENC's input: NV12 (`mode` 0) or planar 4:4:4 (`mode` 1).
const FRAGMENT: &str = r#"#version 330 core
uniform sampler2D src;
uniform vec2 size;
uniform int mode;
uniform vec4 ky;
uniform vec4 ku;
uniform vec4 kv;
out vec4 o;
vec3 at(vec2 px) { return texture(src, px / size).rgb; }
void main() {
    vec2 p = floor(gl_FragCoord.xy);
    float h = size.y;
    float v;
    if (mode == 1) {
        float plane = floor(p.y / h);
        vec3 c = at(vec2(p.x, p.y - plane * h) + 0.5);
        vec4 k = plane < 0.5 ? ky : (plane < 1.5 ? ku : kv);
        v = dot(k.rgb, c) + k.a;
    } else if (p.y < h) {
        v = dot(ky.rgb, at(p + 0.5)) + ky.a;
    } else {
        // Chroma of the 2x2 block: sampling its centre averages the four pixels.
        float cx = floor(p.x * 0.5);
        vec3 c = at(vec2(cx * 2.0 + 1.0, (p.y - h) * 2.0 + 1.0));
        vec4 k = mod(p.x, 2.0) < 0.5 ? ku : kv;
        v = dot(k.rgb, c) + k.a;
    }
    o = vec4(v, 0.0, 0.0, 1.0);
}
"#;

/// Draws one plane of an NV12 picture into a render target of that plane's size:
/// `plane` 0, luma (R8); 1, interleaved chroma (GR88: U in red, V in green).
/// Used for VAAPI surfaces, whose two planes are separate DMA-BUF layers.
const FRAGMENT_PLANES: &str = r#"#version 330 core
uniform sampler2D src;
uniform vec2 size;
uniform int plane;
uniform vec4 ky;
uniform vec4 ku;
uniform vec4 kv;
out vec4 o;
vec3 at(vec2 px) { return texture(src, px / size).rgb; }
void main() {
    vec2 p = floor(gl_FragCoord.xy);
    if (plane == 0) {
        o = vec4(dot(ky.rgb, at(p + 0.5)) + ky.a, 0.0, 0.0, 1.0);
    } else {
        // Chroma of the 2x2 block: sampling its centre averages the four pixels.
        vec3 c = at(p * 2.0 + 1.0);
        o = vec4(dot(ku.rgb, c) + ku.a, dot(kv.rgb, c) + kv.a, 0.0, 1.0);
    }
}
"#;

// ---- Converter -------------------------------------------------------------------

struct Imported {
    image: EglImage,
    tex: glow::Texture,
    planes: Vec<DmabufPlane>,
    modifier: u64,
    size: (u32, u32),
}

/// The R8 render target matching one encoder input buffer, registered with CUDA.
struct Target {
    tex: glow::Texture,
    fbo: glow::Framebuffer,
    res: CuResource,
    ctx: *mut c_void,
    width: usize,
    rows: usize,
    yuv444: bool,
    dev: u64,
}

/// One VAAPI NV12 surface of the encoder, imported as two render targets.
struct VaTarget {
    images: [EglImage; 2],
    texs: [glow::Texture; 2],
    fbos: [glow::Framebuffer; 2],
    size: (usize, usize),
}

/// A VAAPI surface to draw an NV12 picture into (see `export_surface_for_writing`).
pub struct VaSurface<'a> {
    /// Identity of the surface (the encoder recycles a small pool).
    pub id: u32,
    pub width: usize,
    pub height: usize,
    /// The exported layers: luma (`R8`), then chroma (`GR88`).
    pub planes: &'a [crate::vaapi::DmaBufPlane],
}

pub struct GpuConverter {
    egl: Egl,
    /// NVENC's CUDA input (NVIDIA); `None` for VAAPI.
    cuda: Option<Cuda>,
    dpy: EglDisplay,
    ctx: EglContext,
    gl: glow::Context,
    program: glow::Program,
    vao: glow::VertexArray,
    uniforms: [Option<glow::UniformLocation>; 6],
    imported: HashMap<usize, Imported>,
    target: Option<Target>,
    /// NV12 plane program and the imported VAAPI surfaces (VAAPI only).
    planes: Option<(glow::Program, Uniforms)>,
    va_targets: HashMap<u32, VaTarget>,
}

impl GpuConverter {
    /// Set up EGL + OpenGL on the NVIDIA GPU and make the context current on this thread.
    pub fn new() -> Result<Self> {
        let cuda = Cuda::load()?;
        Self::open(Some(cuda), None)
    }

    /// Same for a VAAPI encoder (Intel / AMD): EGL on the GPU of `render_node`
    /// (e.g. `/dev/dri/renderD128`), output through [`Self::convert_to_surface`].
    pub fn new_vaapi(render_node: Option<&str>) -> Result<Self> {
        let mut c = Self::open(None, render_node)?;
        let (program, uniforms) = unsafe { build_planes_program(&c.gl) }.map_err(|e| anyhow!("conversion shader (NV12 planes): {e}"))?;
        c.planes = Some((program, uniforms));
        Ok(c)
    }

    fn open(cuda: Option<Cuda>, render_node: Option<&str>) -> Result<Self> {
        let egl = Egl::load()?;
        let dpy = egl.device_display(render_node)?;
        let (mut major, mut minor) = (0, 0);
        if unsafe { (egl.initialize)(dpy, &mut major, &mut minor) } == 0 {
            bail!("eglInitialize failed (0x{:x})", unsafe { (egl.get_error)() });
        }
        let exts = egl.string(dpy, EGL_EXTENSIONS);
        for need in ["EGL_EXT_image_dma_buf_import_modifiers", "EGL_KHR_surfaceless_context", "EGL_KHR_no_config_context"] {
            if !exts.contains(need) {
                unsafe { (egl.terminate)(dpy) };
                bail!("EGL lacks {need}");
            }
        }
        let vendor = egl.string(dpy, EGL_VENDOR);
        if unsafe { (egl.bind_api)(EGL_OPENGL_API) } == 0 {
            unsafe { (egl.terminate)(dpy) };
            bail!("eglBindAPI(OpenGL) failed");
        }
        let attrs = [EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 3, EGL_CONTEXT_OPENGL_PROFILE_MASK, EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT, EGL_NONE];
        let ctx = unsafe { (egl.create_context)(dpy, std::ptr::null_mut(), std::ptr::null_mut(), attrs.as_ptr()) };
        if ctx.is_null() {
            unsafe { (egl.terminate)(dpy) };
            bail!("eglCreateContext failed (0x{:x})", unsafe { (egl.get_error)() });
        }
        if unsafe { (egl.make_current)(dpy, std::ptr::null_mut(), std::ptr::null_mut(), ctx) } == 0 {
            unsafe {
                (egl.destroy_context)(dpy, ctx);
                (egl.terminate)(dpy);
            }
            bail!("eglMakeCurrent failed (0x{:x})", unsafe { (egl.get_error)() });
        }
        let gl = unsafe { glow::Context::from_loader_function(|s| egl.proc(s)) };
        let (program, vao, uniforms) = unsafe { build_program(&gl) }.map_err(|e| anyhow!("conversion shader: {e}"))?;
        info!("GPU conversion: EGL {major}.{minor} ({vendor}), {}", unsafe { gl.get_parameter_string(glow::RENDERER) });
        Ok(Self { egl, cuda, dpy, ctx, gl, program, vao, uniforms, imported: HashMap::new(), target: None, planes: None, va_targets: HashMap::new() })
    }

    /// DRM modifiers the GPU can sample `fourcc` with (none: not importable).
    pub fn modifiers(&self, fourcc: u32) -> Vec<u64> {
        let mut n = 0;
        unsafe { (self.egl.query_modifiers)(self.dpy, fourcc as i32, 0, std::ptr::null_mut(), std::ptr::null_mut(), &mut n) };
        if n <= 0 {
            return Vec::new();
        }
        let mut mods = vec![0u64; n as usize];
        let mut external = vec![0u32; n as usize];
        unsafe { (self.egl.query_modifiers)(self.dpy, fourcc as i32, n, mods.as_mut_ptr(), external.as_mut_ptr(), &mut n) };
        // External-only layouts need samplerExternalOES, which desktop GL lacks.
        mods.iter().zip(&external).take(n.max(0) as usize).filter(|(_, e)| **e == 0).map(|(m, _)| *m).collect()
    }

    /// Drop every imported buffer (the capture renegotiated its buffers).
    pub fn forget_buffers(&mut self) {
        for (_, i) in self.imported.drain() {
            unsafe {
                self.gl.delete_texture(i.tex);
                (self.egl.destroy_image)(self.dpy, i.image);
            }
        }
    }

    fn import(&mut self, f: &DmabufFrame) -> Result<glow::Texture> {
        if let Some(i) = self.imported.get(&f.key) {
            if i.planes == f.planes && i.modifier == f.modifier && i.size == (f.width, f.height) {
                return Ok(i.tex);
            }
        }
        if let Some(old) = self.imported.remove(&f.key) {
            unsafe {
                self.gl.delete_texture(old.tex);
                (self.egl.destroy_image)(self.dpy, old.image);
            }
        }
        let mut attrs = vec![EGL_WIDTH, f.width as i32, EGL_HEIGHT, f.height as i32, EGL_LINUX_DRM_FOURCC_EXT, f.fourcc as i32, EGL_IMAGE_PRESERVED_KHR, 1];
        for (p, a) in f.planes.iter().zip(EGL_PLANE_ATTRS.iter()) {
            attrs.extend_from_slice(&[a[0], p.fd, a[1], p.offset as i32, a[2], p.stride as i32]);
            if f.modifier != DRM_FORMAT_MOD_INVALID {
                attrs.extend_from_slice(&[a[3], f.modifier as u32 as i32, a[4], (f.modifier >> 32) as u32 as i32]);
            }
        }
        attrs.push(EGL_NONE);
        let image = unsafe { (self.egl.create_image)(self.dpy, std::ptr::null_mut(), EGL_LINUX_DMA_BUF_EXT, std::ptr::null_mut(), attrs.as_ptr()) };
        if image.is_null() {
            bail!("cannot import the DMA-BUF (eglCreateImage error 0x{:x}, modifier 0x{:x})", unsafe { (self.egl.get_error)() }, f.modifier);
        }
        let tex = unsafe {
            let tex = self.gl.create_texture().map_err(|e| anyhow!(e))?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            (self.egl.image_target_texture)(glow::TEXTURE_2D, image);
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            self.gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            tex
        };
        let err = unsafe { self.gl.get_error() };
        if err != glow::NO_ERROR {
            unsafe {
                self.gl.delete_texture(tex);
                (self.egl.destroy_image)(self.dpy, image);
            }
            bail!("binding the DMA-BUF to a texture failed (GL error 0x{err:x})");
        }
        debug!("imported DMA-BUF {:?} {}x{} modifier 0x{:x}", f.planes, f.width, f.height, f.modifier);
        self.imported.insert(f.key, Imported { image, tex, planes: f.planes.clone(), modifier: f.modifier, size: (f.width, f.height) });
        Ok(tex)
    }

    fn target(&mut self, dst: &CudaInput) -> Result<&Target> {
        let rows = if dst.yuv444 { dst.height * 3 } else { dst.height * 3 / 2 };
        let ok = self.target.as_ref().is_some_and(|t| t.width == dst.width && t.rows == rows && t.yuv444 == dst.yuv444 && t.dev == dst.dev && t.ctx == dst.ctx);
        if !ok {
            self.drop_target();
            unsafe {
                let tex = self.gl.create_texture().map_err(|e| anyhow!(e))?;
                self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                self.gl.tex_storage_2d(glow::TEXTURE_2D, 1, glow::R8, dst.width as i32, rows as i32);
                let fbo = self.gl.create_framebuffer().map_err(|e| anyhow!(e))?;
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                self.gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
                let status = self.gl.check_framebuffer_status(glow::FRAMEBUFFER);
                if status != glow::FRAMEBUFFER_COMPLETE {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                    bail!("conversion target incomplete (0x{status:x})");
                }
                let mut res = std::ptr::null_mut();
                let Some(cuda) = self.cuda.as_ref() else {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                    bail!("this converter has no CUDA output");
                };
                let r = cuda.with(dst.ctx, || (cuda.register_image)(&mut res, tex.0.get(), glow::TEXTURE_2D, CU_GRAPHICS_REGISTER_FLAGS_READ_ONLY));
                if r != 0 {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                    bail!("cuGraphicsGLRegisterImage failed (CUDA error {r})");
                }
                self.target = Some(Target { tex, fbo, res, ctx: dst.ctx, width: dst.width, rows, yuv444: dst.yuv444, dev: dst.dev });
            }
        }
        Ok(self.target.as_ref().unwrap())
    }

    fn drop_target(&mut self) {
        if let Some(t) = self.target.take() {
            if let Some(cuda) = self.cuda.as_ref() {
                cuda.with(t.ctx, || unsafe { (cuda.unregister)(t.res) });
            }
            unsafe {
                self.gl.delete_framebuffer(t.fbo);
                self.gl.delete_texture(t.tex);
            }
        }
    }

    /// Convert `f` into the encoder's GPU input buffer `dst`, scaling if sizes differ.
    /// Returns once the copy is complete, so the caller may release the DMA-BUF.
    pub fn convert(&mut self, f: &DmabufFrame, dst: &CudaInput, csc: Csc) -> Result<()> {
        if !dst.width.is_multiple_of(2) || !dst.height.is_multiple_of(2) {
            bail!("GPU conversion needs even picture sizes");
        }
        let src = self.import(f)?;
        let (fbo, res, ctx, width, rows) = {
            let t = self.target(dst)?;
            (t.fbo, t.res, t.ctx, t.width, t.rows)
        };
        let k = csc.yuv_rows();
        unsafe {
            let gl = &self.gl;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.viewport(0, 0, width as i32, rows as i32);
            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(src));
            let u = &self.uniforms;
            gl.uniform_1_i32(u[0].as_ref(), 0);
            gl.uniform_2_f32(u[1].as_ref(), dst.width as f32, dst.height as f32);
            gl.uniform_1_i32(u[2].as_ref(), dst.yuv444 as i32);
            for (loc, row) in u[3..].iter().zip(k.iter()) {
                gl.uniform_4_f32(loc.as_ref(), row[0], row[1], row[2], row[3]);
            }
            // The picture is a different size from the capture: sample in picture
            // pixels, the texture coordinates are normalised so scaling is implicit.
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            let err = gl.get_error();
            if err != glow::NO_ERROR {
                bail!("conversion draw failed (GL error 0x{err:x})");
            }
        }
        // Mapping waits for the GL work; the copy is synchronous, so the DMA-BUF
        // is no longer read once this returns.
        let cuda = self.cuda.as_ref().ok_or_else(|| anyhow!("this converter has no CUDA output"))?;
        cuda.with(ctx, || -> Result<()> {
            let mut r = res;
            cu(unsafe { (cuda.map)(1, &mut r, std::ptr::null_mut()) }, "cuGraphicsMapResources")?;
            let mut arr = std::ptr::null_mut();
            let mut out = cu(unsafe { (cuda.mapped_array)(&mut arr, r, 0, 0) }, "cuGraphicsSubResourceGetMappedArray");
            if out.is_ok() {
                let c = CudaMemcpy2D {
                    src_x_in_bytes: 0,
                    src_y: 0,
                    src_memory_type: CU_MEMORYTYPE_ARRAY,
                    src_host: std::ptr::null(),
                    src_device: 0,
                    src_array: arr,
                    src_pitch: 0,
                    dst_x_in_bytes: 0,
                    dst_y: 0,
                    dst_memory_type: CU_MEMORYTYPE_DEVICE,
                    dst_host: std::ptr::null_mut(),
                    dst_device: dst.dev,
                    dst_array: std::ptr::null_mut(),
                    dst_pitch: dst.pitch,
                    width_in_bytes: width,
                    height: rows,
                };
                out = cu(unsafe { (cuda.memcpy_2d)(&c) }, "cuMemcpy2D");
            }
            unsafe { (cuda.unmap)(1, &mut r, std::ptr::null_mut()) };
            out
        })
    }
}

impl GpuConverter {
    /// Import (once per surface) the two layers of a VAAPI NV12 surface as render targets.
    fn va_target(&mut self, dst: &VaSurface) -> Result<([glow::Framebuffer; 2], (usize, usize))> {
        if let Some(t) = self.va_targets.get(&dst.id) {
            if t.size == (dst.width, dst.height) {
                return Ok((t.fbos, t.size));
            }
        }
        if let Some(old) = self.va_targets.remove(&dst.id) {
            self.drop_va_target(old);
        }
        if dst.planes.len() != 2 {
            bail!("VAAPI surface with {} layers (NV12 expected)", dst.planes.len());
        }
        let sizes = [(dst.width, dst.height), (dst.width / 2, dst.height / 2)];
        let mut images = Vec::new();
        let mut texs = Vec::new();
        let mut fbos = Vec::new();
        let mut fail = None;
        for (p, (w, h)) in dst.planes.iter().zip(sizes) {
            let mut attrs = vec![EGL_WIDTH, w as i32, EGL_HEIGHT, h as i32, EGL_LINUX_DRM_FOURCC_EXT, p.drm_format as i32];
            let a = EGL_PLANE_ATTRS[0];
            attrs.extend_from_slice(&[a[0], p.fd, a[1], p.offset as i32, a[2], p.pitch as i32]);
            if p.modifier != DRM_FORMAT_MOD_INVALID {
                attrs.extend_from_slice(&[a[3], p.modifier as u32 as i32, a[4], (p.modifier >> 32) as u32 as i32]);
            }
            attrs.push(EGL_NONE);
            let image = unsafe { (self.egl.create_image)(self.dpy, std::ptr::null_mut(), EGL_LINUX_DMA_BUF_EXT, std::ptr::null_mut(), attrs.as_ptr()) };
            if image.is_null() {
                fail = Some(anyhow!("cannot import the VAAPI surface (eglCreateImage error 0x{:x}, format 0x{:x}, modifier 0x{:x})", unsafe { (self.egl.get_error)() }, p.drm_format, p.modifier));
                break;
            }
            images.push(image);
            unsafe {
                let tex = match self.gl.create_texture() {
                    Ok(t) => t,
                    Err(e) => {
                        fail = Some(anyhow!(e));
                        break;
                    }
                };
                texs.push(tex);
                self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                (self.egl.image_target_texture)(glow::TEXTURE_2D, image);
                let fbo = match self.gl.create_framebuffer() {
                    Ok(f) => f,
                    Err(e) => {
                        fail = Some(anyhow!(e));
                        break;
                    }
                };
                fbos.push(fbo);
                self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                self.gl.framebuffer_texture_2d(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::TEXTURE_2D, Some(tex), 0);
                let status = self.gl.check_framebuffer_status(glow::FRAMEBUFFER);
                if status != glow::FRAMEBUFFER_COMPLETE {
                    fail = Some(anyhow!("the VAAPI surface cannot be drawn into (framebuffer status 0x{status:x}, format 0x{:x})", p.drm_format));
                    break;
                }
            }
        }
        if let Some(e) = fail {
            unsafe {
                for f in fbos {
                    self.gl.delete_framebuffer(f);
                }
                for t in texs {
                    self.gl.delete_texture(t);
                }
                for i in images {
                    (self.egl.destroy_image)(self.dpy, i);
                }
            }
            return Err(e);
        }
        debug!("imported VAAPI surface {} ({}x{}) as render targets", dst.id, dst.width, dst.height);
        let t = VaTarget { images: [images[0], images[1]], texs: [texs[0], texs[1]], fbos: [fbos[0], fbos[1]], size: (dst.width, dst.height) };
        let out = (t.fbos, t.size);
        self.va_targets.insert(dst.id, t);
        Ok(out)
    }

    fn drop_va_target(&self, t: VaTarget) {
        unsafe {
            for f in t.fbos {
                self.gl.delete_framebuffer(f);
            }
            for tex in t.texs {
                self.gl.delete_texture(tex);
            }
            for i in t.images {
                (self.egl.destroy_image)(self.dpy, i);
            }
        }
    }

    /// Drop the imported VAAPI surfaces (the encoder was reopened).
    pub fn forget_surfaces(&mut self) {
        let all: Vec<VaTarget> = self.va_targets.drain().map(|(_, t)| t).collect();
        for t in all {
            self.drop_va_target(t);
        }
    }

    /// Convert `f` into the VAAPI encoder's NV12 surface `dst`, scaling if sizes
    /// differ. Returns once the GPU is done, so the caller may release the
    /// DMA-BUF and encode the surface.
    pub fn convert_to_surface(&mut self, f: &DmabufFrame, dst: &VaSurface, csc: Csc) -> Result<()> {
        if !dst.width.is_multiple_of(2) || !dst.height.is_multiple_of(2) {
            bail!("GPU conversion needs even picture sizes");
        }
        let (program, u) = self.planes.clone().ok_or_else(|| anyhow!("this converter has no VAAPI output"))?;
        let src = self.import(f)?;
        let (fbos, (w, h)) = self.va_target(dst)?;
        let k = csc.yuv_rows();
        unsafe {
            let gl = &self.gl;
            gl.use_program(Some(program));
            gl.bind_vertex_array(Some(self.vao));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(src));
            gl.uniform_1_i32(u[0].as_ref(), 0);
            gl.uniform_2_f32(u[1].as_ref(), w as f32, h as f32);
            for (loc, row) in u[3..].iter().zip(k.iter()) {
                gl.uniform_4_f32(loc.as_ref(), row[0], row[1], row[2], row[3]);
            }
            for (plane, (fbo, (pw, ph))) in fbos.iter().zip([(w, h), (w / 2, h / 2)]).enumerate() {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(*fbo));
                gl.viewport(0, 0, pw as i32, ph as i32);
                gl.uniform_1_i32(u[2].as_ref(), plane as i32);
                gl.draw_arrays(glow::TRIANGLES, 0, 3);
            }
            let err = gl.get_error();
            if err != glow::NO_ERROR {
                bail!("conversion draw failed (GL error 0x{err:x})");
            }
            // The encoder reads the surface through another API: wait for the GPU.
            gl.finish();
        }
        Ok(())
    }
}

impl Drop for GpuConverter {
    fn drop(&mut self) {
        self.drop_target();
        self.forget_buffers();
        self.forget_surfaces();
        unsafe {
            if let Some((p, _)) = self.planes.take() {
                self.gl.delete_program(p);
            }
            self.gl.delete_program(self.program);
            self.gl.delete_vertex_array(self.vao);
            (self.egl.make_current)(self.dpy, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            (self.egl.destroy_context)(self.dpy, self.ctx);
            (self.egl.terminate)(self.dpy);
        }
    }
}

type Uniforms = [Option<glow::UniformLocation>; 6];

/// The NV12 plane program (uniforms: src, size, plane, ky, ku, kv).
unsafe fn build_planes_program(gl: &glow::Context) -> Result<(glow::Program, Uniforms), String> {
    let program = gl.create_program()?;
    let mut shaders = Vec::new();
    for (kind, src) in [(glow::VERTEX_SHADER, VERTEX), (glow::FRAGMENT_SHADER, FRAGMENT_PLANES)] {
        let s = gl.create_shader(kind)?;
        gl.shader_source(s, src);
        gl.compile_shader(s);
        if !gl.get_shader_compile_status(s) {
            return Err(gl.get_shader_info_log(s));
        }
        gl.attach_shader(program, s);
        shaders.push(s);
    }
    gl.link_program(program);
    if !gl.get_program_link_status(program) {
        return Err(gl.get_program_info_log(program));
    }
    for s in shaders {
        gl.detach_shader(program, s);
        gl.delete_shader(s);
    }
    let u = ["src", "size", "plane", "ky", "ku", "kv"].map(|n| gl.get_uniform_location(program, n));
    Ok((program, u))
}

unsafe fn build_program(gl: &glow::Context) -> Result<(glow::Program, glow::VertexArray, Uniforms), String> {
    let program = gl.create_program()?;
    let mut shaders = Vec::new();
    for (kind, src) in [(glow::VERTEX_SHADER, VERTEX), (glow::FRAGMENT_SHADER, FRAGMENT)] {
        let s = gl.create_shader(kind)?;
        gl.shader_source(s, src);
        gl.compile_shader(s);
        if !gl.get_shader_compile_status(s) {
            return Err(gl.get_shader_info_log(s));
        }
        gl.attach_shader(program, s);
        shaders.push(s);
    }
    gl.link_program(program);
    if !gl.get_program_link_status(program) {
        return Err(gl.get_program_info_log(program));
    }
    for s in shaders {
        gl.detach_shader(program, s);
        gl.delete_shader(s);
    }
    let vao = gl.create_vertex_array()?;
    let u = ["src", "size", "mode", "ky", "ku", "kv"].map(|n| gl.get_uniform_location(program, n));
    Ok((program, vao, u))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs an NVIDIA GPU: `cargo test -p moon-media glconv -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn converter_starts_and_lists_modifiers() {
        let c = GpuConverter::new().expect("GPU converter");
        let m = c.modifiers(DRM_FORMAT_XRGB8888);
        eprintln!("XRGB8888 modifiers: {:x?}", m);
        assert!(!m.is_empty());
    }
}
