# eframe 0.37.0, patched for 2moon

Unmodified eframe 0.37.0 from crates.io (MIT OR Apache-2.0), except:

- `src/lib.rs`: `pub static WAYLAND_NO_FRAME_CALLBACK`.
- `src/native/glow_integration.rs`: `window.pre_present_notify()` is skipped while it is true.

Why: on Wayland, `pre_present_notify` makes winit hold back the next `RedrawRequested`
until the compositor's frame callback, about one display refresh. A decoded stream frame
waited half a refresh on average (8 ms at 60 Hz) before being drawn. The client sets the
flag during a stream with "Latence minimale" on.

To update eframe: copy the new release here and redo these two changes.
