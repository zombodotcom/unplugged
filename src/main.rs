#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Unplugged")
            .with_inner_size([1280.0, 760.0])
            .with_min_inner_size([900.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Unplugged",
        options,
        Box::new(|cc| Ok(Box::new(unplugged::app::App::new(cc)))),
    )
}
