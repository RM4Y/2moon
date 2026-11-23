//! Windows capture (DXGI Desktop Duplication): BGRA pictures, pointer drawn on top.
//! Follows desktop switches (UAC, lock screen) when running as SYSTEM.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use moon_media::frame::I420Frame;
use moon_media::hdr::P010Frame;
use tracing::{info, warn};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};
use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};

use crate::capture::VideoSource;

/// Desktop rectangle of the display being captured (virtual-screen pixels), for
/// absolute mouse positions: (left, top, width, height).
pub static CAPTURED_RECT: Mutex<Option<(i32, i32, i32, i32)>> = Mutex::new(None);

/// A desktop picture in BGRA.
#[derive(Default)]
struct Picture {
    data: Vec<u8>,
    width: usize,
    height: usize,
}

/// The mouse pointer as DXGI reports it.
#[derive(Default, Clone)]
struct Pointer {
    visible: bool,
    x: i32,
    y: i32,
    shape: Option<Arc<PointerShape>>,
}

struct PointerShape {
    kind: i32,
    width: usize,
    /// For monochrome pointers, the AND mask and the XOR mask stacked: twice the height.
    height: usize,
    pitch: usize,
    data: Vec<u8>,
}

#[derive(Default)]
struct Shared {
    /// Newest picture not taken yet.
    latest: Option<Picture>,
    /// A buffer to reuse for the next copy.
    spare: Option<Picture>,
    pointer: Pointer,
    /// Pictures (or pointer moves) delivered, and when the newest one came.
    seq: u64,
    at: Option<Instant>,
    /// Refresh rate of the display (Hz), once known.
    refresh: f64,
    error: Option<String>,
}

struct Inner {
    shared: Mutex<Shared>,
    cond: Condvar,
    stop: AtomicBool,
}

/// `--video dxgi`: a display of this computer.
pub struct DxgiSource {
    output: String,
    inner: Option<Arc<Inner>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Last picture taken (kept to re-encode when nothing changed).
    current: Picture,
    pointer: Pointer,
    last_seq: u64,
    warned: bool,
}

impl DxgiSource {
    /// `output`: part of the display's device name (e.g. `DISPLAY2`); empty = the first one.
    pub fn new(output: String) -> Self {
        Self { output, inner: None, thread: None, current: Picture::default(), pointer: Pointer::default(), last_seq: 0, warned: false }
    }

    fn inner(&mut self) -> Result<Arc<Inner>> {
        if self.inner.is_none() {
            self.start(60, &[])?;
        }
        Ok(self.inner.clone().unwrap())
    }

    /// Take the newest picture if there is one; the previous goes back for reuse.
    fn take_new(&mut self, wait: Duration) -> Result<bool> {
        let inner = self.inner()?;
        let deadline = Instant::now() + wait;
        let mut sh = inner.shared.lock().unwrap();
        loop {
            if sh.seq > self.last_seq {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            sh = inner.cond.wait_timeout(sh, deadline - now).unwrap().0;
        }
        self.last_seq = sh.seq;
        self.pointer = sh.pointer.clone();
        if let Some(p) = sh.latest.take() {
            let old = std::mem::replace(&mut self.current, p);
            if !old.data.is_empty() {
                sh.spare = Some(old);
            }
        }
        Ok(true)
    }

    /// Convert the current picture with the pointer drawn on it.
    fn with_picture(&mut self, f: impl FnOnce(&[u8], usize, usize)) -> bool {
        let (w, h) = (self.current.width, self.current.height);
        if self.current.data.is_empty() {
            return false;
        }
        let saved = draw_pointer(&mut self.current, &self.pointer);
        f(&self.current.data, w, h);
        if let Some(saved) = saved {
            saved.restore(&mut self.current);
        }
        true
    }

    fn no_picture_yet(&mut self) {
        if !self.warned {
            let err = self.inner.as_ref().and_then(|i| i.shared.lock().unwrap().error.clone());
            warn!("no picture from the display yet{}", err.map(|e| format!(": {e}")).unwrap_or_default());
            self.warned = true;
        }
    }
}

impl Drop for DxgiSource {
    fn drop(&mut self) {
        if let Some(i) = &self.inner {
            i.stop.store(true, Ordering::Relaxed);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl VideoSource for DxgiSource {
    fn name(&self) -> &'static str {
        "dxgi"
    }

    fn start(&mut self, _fps: u32, _dmabuf: &[(u32, Vec<u64>)]) -> Result<()> {
        if self.inner.is_some() {
            return Ok(());
        }
        let inner = Arc::new(Inner { shared: Mutex::new(Shared::default()), cond: Condvar::new(), stop: AtomicBool::new(false) });
        let (i, output) = (inner.clone(), self.output.clone());
        self.thread = Some(std::thread::Builder::new().name("dxgi-capture".into()).spawn(move || capture_thread(&i, &output))?);
        self.inner = Some(inner);
        Ok(())
    }

    fn grab(&mut self, out: &mut I420Frame, w: usize, h: usize) -> Result<()> {
        let have_previous = !self.current.data.is_empty();
        // Re-encode the previous picture rather than stall when nothing changed.
        self.take_new(if have_previous { Duration::ZERO } else { Duration::from_millis(200) })?;
        if !self.with_picture(|data, sw, sh| out.from_rgb32_scaled(data, sw, sh, sw * 4, false, w, h)) {
            self.no_picture_yet();
            if out.width != w || out.height != h {
                let (csc, yuv444) = (out.csc, out.yuv444);
                *out = I420Frame::with_format(w, h, yuv444);
                out.csc = csc;
            }
        }
        Ok(())
    }

    fn grab_hdr10(&mut self, out: &mut P010Frame, w: usize, h: usize) -> Result<()> {
        let have_previous = !self.current.data.is_empty();
        self.take_new(if have_previous { Duration::ZERO } else { Duration::from_millis(200) })?;
        if !self.with_picture(|data, sw, sh| out.from_rgb32_scaled(data, sw, sh, sw * 4, false, w, h)) && (out.width != w || out.height != h) {
            *out = P010Frame::new(w, h);
        }
        Ok(())
    }

    fn wait_frame(&mut self, _due: Instant, deadline: Instant) -> bool {
        let Ok(inner) = self.inner() else { return false };
        let mut sh = inner.shared.lock().unwrap();
        while sh.seq <= self.last_seq {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            sh = inner.cond.wait_timeout(sh, deadline - now).unwrap().0;
        }
        true
    }

    fn pending_arrival(&mut self) -> Option<Instant> {
        let inner = self.inner.clone()?;
        let sh = inner.shared.lock().unwrap();
        if sh.seq > self.last_seq { sh.at } else { None }
    }

    fn wait_arrival_after(&mut self, after: Instant, deadline: Instant) -> bool {
        let Ok(inner) = self.inner() else { return false };
        let mut sh = inner.shared.lock().unwrap();
        loop {
            if sh.seq > self.last_seq && sh.at.is_some_and(|a| a >= after) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            sh = inner.cond.wait_timeout(sh, deadline - now).unwrap().0;
        }
    }

    fn pictures_received(&self) -> Option<u64> {
        Some(self.inner.as_ref()?.shared.lock().unwrap().seq)
    }

    fn source_fps(&self) -> Option<f64> {
        let r = self.inner.as_ref()?.shared.lock().unwrap().refresh;
        // Pictures come when the screen changes, at most at the refresh rate.
        Some(if r > 0.0 { r } else { 0.0 })
    }
}

// ------------------------------------------------------------- capture thread

struct Duplication {
    dup: IDXGIOutputDuplication,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    /// Desktop position of the display.
    origin: (i32, i32),
}

fn capture_thread(inner: &Inner, output: &str) {
    let mut dup: Option<Duplication> = None;
    let mut pointer_buf: Vec<u8> = Vec::new();
    let mut last_error = String::new();
    while !inner.stop.load(Ordering::Relaxed) {
        let d = match dup.as_mut() {
            Some(d) => d,
            None => {
                // Follow the desktop that receives input (normal, UAC, lock screen).
                crate::win::session::follow_input_desktop();
                match open_duplication(output) {
                    Ok((d, name, refresh)) => {
                        info!("capturing {name} ({}x{} at {refresh:.0} Hz) with DXGI Desktop Duplication", unsafe { d.dup.GetDesc() }.ModeDesc.Width, unsafe { d.dup.GetDesc() }.ModeDesc.Height);
                        last_error.clear();
                        let mut sh = inner.shared.lock().unwrap();
                        sh.refresh = refresh;
                        sh.error = None;
                        drop(sh);
                        dup = Some(d);
                        continue;
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        if msg != last_error {
                            warn!("display capture: {msg}");
                            last_error = msg.clone();
                        }
                        inner.shared.lock().unwrap().error = Some(msg);
                        std::thread::sleep(Duration::from_millis(250));
                        continue;
                    }
                }
            }
        };
        match next_frame(d, inner, &mut pointer_buf) {
            Ok(()) => {}
            Err(e) => {
                let lost = e.downcast_ref::<windows::core::Error>().is_some_and(|w| w.code() == DXGI_ERROR_ACCESS_LOST);
                if !lost {
                    warn!("display capture: {e:#}");
                    std::thread::sleep(Duration::from_millis(100));
                }
                // Lost (desktop switch, mode change, full-screen app): start again.
                dup = None;
            }
        }
    }
}

/// The display to capture: its duplication, name and refresh rate.
fn open_duplication(want: &str) -> Result<(Duplication, String, f64)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            a += 1;
            let mut o = 0;
            while let Ok(out) = adapter.EnumOutputs(o) {
                o += 1;
                let desc = out.GetDesc()?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let name = String::from_utf16_lossy(&desc.DeviceName).trim_end_matches('\0').to_string();
                if !want.is_empty() && !name.to_lowercase().contains(&want.to_lowercase()) {
                    continue;
                }
                let r = desc.DesktopCoordinates;
                *CAPTURED_RECT.lock().unwrap() = Some((r.left, r.top, r.right - r.left, r.bottom - r.top));
                let d = duplicate(&adapter, &out.cast::<IDXGIOutput1>()?, (r.left, r.top)).with_context(|| format!("cannot capture {name}"))?;
                return Ok((d, name.clone(), refresh_rate(&desc.DeviceName)));
            }
        }
        if want.is_empty() {
            bail!("no display attached to the desktop");
        }
        bail!("no display named like '{want}'")
    }
}

unsafe fn duplicate(adapter: &IDXGIAdapter1, out: &IDXGIOutput1, origin: (i32, i32)) -> Result<Duplication> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(adapter, D3D_DRIVER_TYPE_UNKNOWN, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None, D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut context)).context("D3D11CreateDevice")?;
        let device: ID3D11Device = device.context("no D3D11 device")?;
        let dup = out.DuplicateOutput(&device).context("DuplicateOutput")?;
        Ok(Duplication { dup, device, context: context.context("no D3D11 context")?, staging: None, origin })
    }
}

fn refresh_rate(device_name: &[u16; 32]) -> f64 {
    let mut mode = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
    let ok = unsafe { EnumDisplaySettingsW(windows::core::PCWSTR(device_name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) };
    if ok.as_bool() && mode.dmDisplayFrequency > 1 { mode.dmDisplayFrequency as f64 } else { 60.0 }
}

/// Wait for the next desktop update (picture or pointer) and publish it.
fn next_frame(d: &mut Duplication, inner: &Inner, pointer_buf: &mut Vec<u8>) -> Result<()> {
    let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
    let mut resource: Option<IDXGIResource> = None;
    match unsafe { d.dup.AcquireNextFrame(100, &mut info, &mut resource) } {
        Ok(()) => {}
        Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    let arrived = Instant::now();
    let result = (|| -> Result<()> {
        let new_picture = info.LastPresentTime != 0 && info.AccumulatedFrames > 0;
        let mut picture = None;
        if new_picture {
            let tex: ID3D11Texture2D = resource.as_ref().context("no desktop picture")?.cast()?;
            let spare = inner.shared.lock().unwrap().spare.take().unwrap_or_default();
            picture = Some(read_back(d, &tex, spare)?);
        }
        // Pointer: new shape and/or position.
        let mut shape = None;
        if info.PointerShapeBufferSize > 0 {
            pointer_buf.resize(info.PointerShapeBufferSize as usize, 0);
            let mut needed = 0u32;
            let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            unsafe { d.dup.GetFramePointerShape(pointer_buf.len() as u32, pointer_buf.as_mut_ptr().cast(), &mut needed, &mut si)? };
            shape = Some(Arc::new(PointerShape { kind: si.Type as i32, width: si.Width as usize, height: si.Height as usize, pitch: si.Pitch as usize, data: pointer_buf[..needed as usize].to_vec() }));
        }
        let pointer_moved = info.LastMouseUpdateTime != 0;
        if picture.is_none() && shape.is_none() && !pointer_moved {
            return Ok(());
        }
        let mut sh = inner.shared.lock().unwrap();
        if let Some(p) = picture {
            if let Some(old) = sh.latest.replace(p) {
                sh.spare = Some(old);
            }
        }
        if let Some(s) = shape {
            sh.pointer.shape = Some(s);
        }
        if pointer_moved {
            let p = info.PointerPosition;
            sh.pointer.visible = p.Visible.as_bool();
            sh.pointer.x = p.Position.x;
            sh.pointer.y = p.Position.y;
        }
        sh.seq += 1;
        sh.at = Some(arrived);
        drop(sh);
        inner.cond.notify_all();
        Ok(())
    })();
    unsafe {
        let _ = d.dup.ReleaseFrame();
    }
    let _ = d.origin;
    result
}

/// Copy the desktop texture into `pic` through a CPU-readable texture.
fn read_back(d: &mut Duplication, tex: &ID3D11Texture2D, mut pic: Picture) -> Result<Picture> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    if d.staging.as_ref().map(|s| (s.1, s.2)) != Some((desc.Width, desc.Height)) {
        let sd = D3D11_TEXTURE2D_DESC {
            Width: desc.Width,
            Height: desc.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut t = None;
        unsafe { d.device.CreateTexture2D(&sd, None, Some(&mut t)).context("CreateTexture2D (staging)")? };
        d.staging = Some((t.context("no staging texture")?, desc.Width, desc.Height));
    }
    let (staging, w, h) = d.staging.clone().unwrap();
    let (w, h) = (w as usize, h as usize);
    unsafe {
        d.context.CopyResource(&staging, tex);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        d.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m)).context("Map")?;
        pic.data.resize(w * h * 4, 0);
        let src = m.pData as *const u8;
        for y in 0..h {
            std::ptr::copy_nonoverlapping(src.add(y * m.RowPitch as usize), pic.data.as_mut_ptr().add(y * w * 4), w * 4);
        }
        d.context.Unmap(&staging, 0);
    }
    pic.width = w;
    pic.height = h;
    Ok(pic)
}

// ------------------------------------------------------------------ pointer

/// Pixels under the pointer, to put back after conversion.
struct Saved {
    x: usize,
    y: usize,
    w: usize,
    rows: Vec<Vec<u8>>,
}

impl Saved {
    fn restore(self, pic: &mut Picture) {
        for (i, row) in self.rows.into_iter().enumerate() {
            let o = ((self.y + i) * pic.width + self.x) * 4;
            pic.data[o..o + self.w * 4].copy_from_slice(&row);
        }
    }
}

/// Draw the pointer on the picture; returns the pixels it covered.
fn draw_pointer(pic: &mut Picture, p: &Pointer) -> Option<Saved> {
    let shape = p.shape.as_ref()?;
    if !p.visible || pic.data.is_empty() {
        return None;
    }
    let mono = shape.kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0;
    let sh_h = if mono { shape.height / 2 } else { shape.height };
    // Visible part of the pointer on the picture.
    let x0 = p.x.max(0) as usize;
    let y0 = p.y.max(0) as usize;
    let x1 = ((p.x + shape.width as i32).max(0) as usize).min(pic.width);
    let y1 = ((p.y + sh_h as i32).max(0) as usize).min(pic.height);
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    let w = x1 - x0;
    let saved = Saved { x: x0, y: y0, w, rows: (y0..y1).map(|y| pic.data[(y * pic.width + x0) * 4..(y * pic.width + x1) * 4].to_vec()).collect() };
    for y in y0..y1 {
        let sy = (y as i32 - p.y) as usize;
        for x in x0..x1 {
            let sx = (x as i32 - p.x) as usize;
            let o = (y * pic.width + x) * 4;
            let dst = &mut pic.data[o..o + 4];
            if mono {
                let bit = |row: usize| (shape.data[row * shape.pitch + sx / 8] >> (7 - sx % 8)) & 1;
                let (and, xor) = (bit(sy), bit(sy + sh_h));
                for c in &mut dst[..3] {
                    let v = if and == 1 { *c } else { 0 };
                    *c = if xor == 1 { v ^ 0xff } else { v };
                }
            } else {
                let s = &shape.data[sy * shape.pitch + sx * 4..sy * shape.pitch + sx * 4 + 4];
                if shape.kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 {
                    let a = s[3] as u32;
                    for c in 0..3 {
                        dst[c] = ((s[c] as u32 * a + dst[c] as u32 * (255 - a)) / 255) as u8;
                    }
                } else if shape.kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 {
                    // Alpha 0: replace with the colour; 0xFF: XOR the screen with it.
                    for c in 0..3 {
                        dst[c] = if s[3] == 0 { s[c] } else { dst[c] ^ s[c] };
                    }
                }
            }
        }
    }
    Some(saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_pointer_is_drawn_and_restored() {
        let mut pic = Picture { data: vec![10; 8 * 8 * 4], width: 8, height: 8 };
        let shape = PointerShape { kind: DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0, width: 2, height: 2, pitch: 8, data: vec![255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0] };
        let p = Pointer { visible: true, x: 7, y: 7, shape: Some(Arc::new(shape)) };
        let before = pic.data.clone();
        let saved = draw_pointer(&mut pic, &p).unwrap();
        assert_eq!(&pic.data[(7 * 8 + 7) * 4..(7 * 8 + 7) * 4 + 3], &[255, 255, 255]);
        saved.restore(&mut pic);
        assert_eq!(pic.data, before);
    }
}
