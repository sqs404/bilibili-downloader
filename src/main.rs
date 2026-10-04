#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod bilibili;
mod dl;

use eframe::egui;

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([920.0, 660.0])
            .with_min_inner_size([780.0, 560.0])
            .with_title("B站视频高清下载器"),
        ..Default::default()
    };
    eframe::run_native(
        "B站视频高清下载器",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
