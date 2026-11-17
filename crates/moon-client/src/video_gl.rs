//! GPU video presentation for the egui client: the decoded YUV 4:2:0 planes
//! are uploaded as three single-channel textures and converted to RGB in a
//! fragment shader, drawn through an egui paint callback.
//!
//! Compared with converting on the CPU this removes, per frame, the colour
//! conversion and three full-size RGB copies (RGB -> RGBA -> egui image ->
//! texture upload), and uploads 1.5 bytes/pixel instead of 4.

use std::sync::{Arc, Mutex};

use eframe::egui;
use eframe::egui_glow;
use eframe::glow::{self, HasContext};
use moon_media::frame::I420Frame;
use tracing::warn;

/// Frames exchanged between the decode thread and the paint callback.
#[derive(Default)]
struct Exchange {
    /// Stream generation: frames from an older stream are ignored.
    generation: u64,
    /// Newest decoded frame not yet uploaded (latest wins).
    pending: Option<I420Frame>,
    /// A buffer to recycle for the next copy.
    spare: Option<I420Frame>,
    /// Size of the latest frame (0 before the first one).
    size: [usize; 2],
    /// Frames uploaded to the GPU (= frames presented).
    presented: u64,
}

#[derive(Clone, Default)]
pub struct VideoView {
    ex: Arc<Mutex<Exchange>>,
    gl: Arc<Mutex<Option<Renderer>>>,
}

impl VideoView {
    /// Start a new stream: drop previous frames, return the sink generation.
    pub fn reset(&self) -> u64 {
        let mut ex = self.ex.lock().unwrap();
        ex.generation += 1;
        ex.pending = None;
        ex.size = [0, 0];
        ex.presented = 0;
        ex.generation
    }

    /// Called on the decode thread for every decoded picture.
    pub fn put(&self, generation: u64, f: &I420Frame) {
        let mut buf = {
            let mut ex = self.ex.lock().unwrap();
            if ex.generation != generation {
                return;
            }
            ex.spare.take().unwrap_or_else(|| I420Frame::new(2, 2))
        };
        buf.copy_from(f);
        let mut ex = self.ex.lock().unwrap();
        if ex.generation != generation {
            return;
        }
        ex.size = [f.width, f.height];
        if let Some(old) = ex.pending.replace(buf) {
            // The previous frame was never shown: the display is behind, skip it.
            ex.spare = Some(old);
        }
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
        let frame = self.ex.lock().unwrap().pending.take();
        if let Some(f) = frame {
            unsafe { r.upload(gl, &f) };
            let mut ex = self.ex.lock().unwrap();
            ex.presented += 1;
            ex.spare = Some(f);
        }
        unsafe { r.draw(gl) };
    }

    /// Free GL resources (call with the context current, e.g. from `on_exit`).
    pub fn destroy(&self, gl: &glow::Context) {
        if let Some(r) = self.gl.lock().unwrap().take() {
            unsafe { r.destroy(gl) };
        }
    }
}

struct Renderer {
    program: glow::Program,
    vbo: glow::Buffer,
    vao: Option<glow::VertexArray>,
    a_pos: u32,
    tex: [glow::Texture; 3],
    u_csc: Option<glow::UniformLocation>,
    u_off: Option<glow::UniformLocation>,
    size: [usize; 2],
    csc: Option<moon_media::frame::Csc>,
    /// R8/RED textures (GL3 / GLES3) or LUMINANCE (GL2 / GLES2).
    modern: bool,
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
        let fs = format!(
            "{head}uniform sampler2D u_y;\nuniform sampler2D u_u;\nuniform sampler2D u_v;\nuniform mat3 u_csc;\nuniform vec3 u_off;\n{fin} vec2 v_uv;\n{out_decl}void main() {{\n  vec3 yuv = vec3({tex}(u_y, v_uv).r, {tex}(u_u, v_uv).r, {tex}(u_v, v_uv).r) - u_off;\n  {out_var} = vec4(clamp(u_csc * yuv, 0.0, 1.0), 1.0);\n}}\n"
        );
        let program = gl.create_program()?;
        let mut shaders = Vec::new();
        for (kind, src) in [(glow::VERTEX_SHADER, &vs), (glow::FRAGMENT_SHADER, &fs)] {
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
        for (i, n) in ["u_y", "u_u", "u_v"].iter().enumerate() {
            gl.uniform_1_i32(gl.get_uniform_location(program, n).as_ref(), i as i32);
        }
        let u_csc = gl.get_uniform_location(program, "u_csc");
        let u_off = gl.get_uniform_location(program, "u_off");

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

        let mut tex = Vec::new();
        for _ in 0..3 {
            let t = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(t));
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            tex.push(t);
        }
        gl.bind_texture(glow::TEXTURE_2D, None);
        Ok(Self { program, vbo, vao, a_pos: 0, tex: [tex[0], tex[1], tex[2]], u_csc, u_off, size: [0, 0], csc: None, modern })
    }

    unsafe fn upload(&mut self, gl: &glow::Context, f: &I420Frame) {
        let (w, h) = (f.width, f.height);
        let (cw, ch) = (f.chroma_width(), h.div_ceil(2));
        let realloc = self.size != [w, h];
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
        if self.csc != Some(f.csc) {
            let (m, off) = f.csc.rgb_matrix();
            gl.use_program(Some(self.program));
            gl.uniform_matrix_3_f32_slice(self.u_csc.as_ref(), false, &m);
            gl.uniform_3_f32(self.u_off.as_ref(), off[0], off[1], off[2]);
            self.csc = Some(f.csc);
        }
    }

    unsafe fn draw(&self, gl: &glow::Context) {
        if self.size[0] == 0 {
            return;
        }
        gl.disable(glow::BLEND);
        gl.use_program(Some(self.program));
        for (i, t) in self.tex.iter().enumerate() {
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
        for i in (0..3).rev() {
            gl.active_texture(glow::TEXTURE0 + i as u32);
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    unsafe fn destroy(self, gl: &glow::Context) {
        gl.delete_program(self.program);
        gl.delete_buffer(self.vbo);
        if let Some(v) = self.vao {
            gl.delete_vertex_array(v);
        }
        for t in self.tex {
            gl.delete_texture(t);
        }
    }
}
