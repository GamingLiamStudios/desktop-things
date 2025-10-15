use desktop_things::layershell;
use egui::ViewportBuilder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fmt_subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_max_level(tracing::Level::TRACE)
        .finish();
    tracing::subscriber::set_global_default(fmt_subscriber)?;

    layershell::run(ui)?;
    Ok(())
}

fn ui(ctx: &egui::Context) {
    egui::CentralPanel::default()
        .frame(egui::Frame::new())
        .show(ctx, |ui| {
            ui.add(egui::Label::new("Test"));
        });

    ctx.request_repaint_after_secs(1.0);
}
