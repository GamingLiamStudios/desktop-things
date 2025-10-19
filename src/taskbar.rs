use std::f64;

use desktop_things::{
    Program,
    RenderContext,
    layershell,
};
use parley::{
    Alignment,
    AlignmentOptions,
    GenericFamily,
    LineHeight,
    StyleProperty,
};
use tracing::debug;
use vello::{
    kurbo::{
        Affine,
        Vec2,
    },
    peniko::{
        Brush,
        Color,
        color::palette,
    },
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fmt_subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::set_global_default(fmt_subscriber)?;

    layershell::run(|| Taskbar {})?;
    Ok(())
}

struct Taskbar {}

impl Program for Taskbar {
    fn draw(
        &mut self,
        ctx: &mut RenderContext,
    ) {
        let time = ctx.current_time.as_secs_f64();

        ctx.add_text(
            |font_context, layout_context, viewport_info| {
                let text = "Aura, Kill Yourself.";

                let mut builder = layout_context.ranged_builder(
                    font_context,
                    text,
                    viewport_info.pixel_scale,
                    true,
                );

                let brush_style = StyleProperty::Brush(Color::WHITE.into());
                builder.push_default(brush_style);

                builder.push_default(GenericFamily::SystemUi);
                builder.push_default(LineHeight::FontSizeRelative(1.3));
                builder.push_default(StyleProperty::FontSize(16.0));

                let mut layout = builder.build(text);

                #[allow(clippy::cast_precision_loss)]
                let max_advance = Some(viewport_info.window_size.1 as f32);
                layout.break_all_lines(max_advance);
                layout.align(max_advance, Alignment::Start, AlignmentOptions::default());

                layout
            },
            |width, height| {
                //debug!(width, height);
                Affine::translate((-f64::from(width) / 2.0, -f64::from(height) / 2.0))
                    .then_rotate(f64::consts::FRAC_PI_2)
                    .then_translate(Vec2::new(f64::from(height) / 2.0, f64::from(width) / 2.0))
            },
        );

        //ctx.request_redraw();
    }
}
