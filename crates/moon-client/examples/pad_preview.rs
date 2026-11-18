//! Renders the controller view in three states and saves a screenshot
//! (PAD_PREVIEW=out.png), to check the drawing without a controller.
use eframe::egui;
use moon_client::padmap::Control;
use moon_proto::control::input::{pad, Gamepad};

struct App {
    frames: u32,
    out: String,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.frames == 0 { moon_client::theme::install_fonts(&ctx); }
        egui::CentralPanel::default().frame(egui::Frame::new().fill(moon_client::theme::SURFACE).inner_margin(16)).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                let states = [
                    ("On demande RB", Gamepad::default(), Some(Control::RB)),
                    ("Appuis : A, LB, croix ↑, LT à moitié, stick G à droite", Gamepad { buttons: pad::A | pad::LB | pad::UP, left_trigger: 128, ls_x: 32767, rs_y: -20000, ..Default::default() }, Some(Control::LeftX)),
                    ("On demande LT · Start appuyé", Gamepad { buttons: pad::START | pad::RB, ..Default::default() }, Some(Control::LT)),
                ];
                for (label, g, ask) in states {
                    ui.vertical(|ui| {
                        ui.set_width(380.0);
                        ui.label(egui::RichText::new(label).color(moon_client::theme::TEXT2));
                        moon_client::padview::paint(ui, &g, ask, 0.4);
                    });
                }
            });
        });
        self.frames += 1;
        if self.frames == 5 {
            let (out, c) = (self.out.clone(), ctx.clone());
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                let [w, h] = img.size;
                let px: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                image::save_buffer(&out, &px, w as u32, h as u32, image::ColorType::Rgba8).unwrap();
                println!("saved {out}");
                c.send_viewport_cmd(egui::ViewportCommand::Close);
                c.request_repaint();
            })));
        }
        ctx.request_repaint();
    }
}

fn main() {
    let out = std::env::var("PAD_PREVIEW").unwrap_or("pad_preview.png".into());
    let opts = eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 330.0]), ..Default::default() };
    eframe::run_native("pad preview", opts, Box::new(|_| Ok(Box::new(App { frames: 0, out })))).unwrap();
}
