//! ScreenCaptureKit (macOS 12.3+): display pictures and system audio (13+).
//! One `SCStream` per use; the callback copies what it needs from each buffer.

use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_media::{CMSampleBuffer, CMTime, CMTimeFlags};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType};

/// What to do with each sample buffer (called on ScreenCaptureKit's queue).
pub type Handler = Box<dyn Fn(&CMSampleBuffer, SCStreamOutputType) + Send + Sync>;

pub struct OutputIvars {
    handler: Handler,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "MoonStreamOutput"]
    #[ivars = OutputIvars]
    pub struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            (self.ivars().handler)(sample, kind);
        }
    }
);

impl StreamOutput {
    fn new(handler: Handler) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { handler });
        unsafe { msg_send![super(this), init] }
    }
}

/// A display ScreenCaptureKit can capture.
pub struct Display {
    pub display: Retained<SCDisplay>,
    pub id: u32,
}

/// Keeps Objective-C objects that ScreenCaptureKit lets any thread use.
struct SendBox<T>(T);
unsafe impl<T> Send for SendBox<T> {}

/// The displays of this Mac (asks for the Screen Recording permission the first time).
pub fn displays() -> Result<Vec<Display>> {
    let (tx, rx) = mpsc::channel::<SendBox<Result<Retained<SCShareableContent>, String>>>();
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let r = match unsafe { Retained::retain(content) } {
            Some(c) => Ok(c),
            None => Err(unsafe { err.as_ref() }.map(|e| e.localizedDescription().to_string()).unwrap_or_else(|| "unknown error".into())),
        };
        let _ = tx.send(SendBox(r));
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    let content = rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| anyhow!("ScreenCaptureKit did not answer"))?
        .0
        .map_err(|e| {
            // Ask only when capture really fails: a service is often told "not
            // allowed" by the preflight check while ScreenCaptureKit works.
            request_screen_capture();
            anyhow!("screen capture refused ({e}): allow moon-server in System Settings › Privacy & Security › Screen Recording")
        })?;
    let list = unsafe { content.displays() };
    Ok(list.iter().map(|d| Display { id: unsafe { d.displayID() }, display: d }).collect())
}

/// A running `SCStream`; stopped when dropped.
pub struct Capture {
    stream: SendBox<Retained<SCStream>>,
    _output: SendBox<Retained<StreamOutput>>,
    _queue: SendBox<DispatchRetained<DispatchQueue>>,
}

/// How to configure a stream.
pub struct Config {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    pub audio: Option<(u32, u32)>,
    pub video: bool,
}

impl Capture {
    pub fn start(display: &Display, cfg: &Config, handler: Handler) -> Result<Self> {
        unsafe {
            let filter = SCContentFilter::initWithDisplay_excludingWindows(SCContentFilter::alloc(), &display.display, &NSArray::new());
            let sc = SCStreamConfiguration::new();
            sc.setWidth(cfg.width.max(2));
            sc.setHeight(cfg.height.max(2));
            sc.setPixelFormat(objc2_core_video::kCVPixelFormatType_32BGRA);
            sc.setShowsCursor(true);
            // Fill the whole picture, as the CPU scaler did: absolute mouse
            // positions map the stream onto the whole display (macOS 14+).
            if sc.respondsToSelector(objc2::sel!(setPreservesAspectRatio:)) {
                sc.setPreservesAspectRatio(false);
            }
            sc.setQueueDepth(if cfg.video { 5 } else { 3 });
            let fps = if cfg.video { cfg.fps.max(1) } else { 1 };
            sc.setMinimumFrameInterval(CMTime { value: 1, timescale: fps as i32, flags: CMTimeFlags::Valid, epoch: 0 });
            if let Some((rate, channels)) = cfg.audio {
                sc.setCapturesAudio(true);
                sc.setSampleRate(rate as isize);
                sc.setChannelCount(channels as isize);
                sc.setExcludesCurrentProcessAudio(true);
            }
            let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter, &sc, None);
            let output = StreamOutput::new(handler);
            let queue = DispatchQueue::new("moon.capture", DispatchQueueAttr::SERIAL);
            let proto = ProtocolObject::from_ref(&*output);
            if cfg.video {
                stream.addStreamOutput_type_sampleHandlerQueue_error(proto, SCStreamOutputType::Screen, Some(&queue)).map_err(|e| anyhow!("addStreamOutput: {}", e.localizedDescription()))?;
            }
            if cfg.audio.is_some() {
                stream.addStreamOutput_type_sampleHandlerQueue_error(proto, SCStreamOutputType::Audio, Some(&queue)).map_err(|e| anyhow!("addStreamOutput (audio): {}", e.localizedDescription()))?;
            }
            let (tx, rx) = mpsc::channel::<Option<String>>();
            let done = RcBlock::new(move |err: *mut NSError| {
                let _ = tx.send(err.as_ref().map(|e| e.localizedDescription().to_string()));
            });
            stream.startCaptureWithCompletionHandler(Some(&done));
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(None) => {}
                Ok(Some(e)) => bail!("cannot start screen capture: {e}"),
                Err(_) => bail!("screen capture did not start"),
            }
            Ok(Self { stream: SendBox(stream), _output: SendBox(output), _queue: SendBox(queue) })
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe { self.stream.0.stopCaptureWithCompletionHandler(None) };
    }
}

/// Shows macOS's Screen Recording dialog (once per run; it points to System Settings).
fn request_screen_capture() {
    static ASKED: std::sync::Once = std::sync::Once::new();
    extern "C" {
        fn CGRequestScreenCaptureAccess() -> bool;
    }
    ASKED.call_once(|| unsafe {
        CGRequestScreenCaptureAccess();
    });
}

/// Size in pixels and refresh rate of a display.
pub fn display_mode(id: u32) -> (usize, usize, f64) {
    use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode};
    let mode = CGDisplayCopyDisplayMode(id);
    let m = mode.as_deref();
    (CGDisplayMode::pixel_width(m), CGDisplayMode::pixel_height(m), CGDisplayMode::refresh_rate(m))
}
