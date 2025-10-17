use desktop_things::layershell;
use tracing::debug;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fmt_subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::set_global_default(fmt_subscriber)?;

    layershell::run(ui)?;
    Ok(())
}

fn ui(ctx: &egui::Context) {
    let frame = egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(0, 4))
        .outer_margin(egui::Margin::symmetric(2, 2))
        .corner_radius(egui::CornerRadius::same(10))
        .fill(egui::Color32::TRANSPARENT.blend(egui::Color32::from_gray(50)));

    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        ui.add(egui::Label::new("Test"));

        if ui.add(egui::Button::new("atoms")).clicked() {
            debug!("WHAT");
        }
    });
}
