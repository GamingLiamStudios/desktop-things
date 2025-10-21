use std::f64;

use desktop_things::{
    FrameInfo,
    InputEvent,
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
        Stroke,
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

    layershell::run(|| Taskbar {
        pointer:  None,
        hovering: false,
    })?;
    Ok(())
}

struct Taskbar {
    pointer:  Option<(f64, f64)>,
    hovering: bool,
}

impl Program for Taskbar {
    fn draw(
        &mut self,
        ctx: &mut RenderContext,
    ) {
        let time = ctx.current_time.as_secs_f64();

        let frame_info = FrameInfo::new(palette::css::BLACK)
            .with_padding((1.0, 3.0))
            .with_margin((5.0, 10.0))
            .with_radius(4.0);

        let text_bounds = ctx.frame(
            &if self.hovering {
                frame_info.with_border(Stroke::default(), palette::css::GRAY)
            } else {
                frame_info
            },
            |_, _| Affine::IDENTITY,
            |ctx| {
                ctx.add_text(
                    |font_context, layout_context, viewport_info| {
                        let text = "Aura, Kill Yourself.";

                        #[allow(clippy::cast_possible_truncation)]
                        let mut builder = layout_context.ranged_builder(
                            font_context,
                            text,
                            viewport_info.pixel_scale as f32,
                            true,
                        );

                        let brush_style = StyleProperty::Brush(Color::WHITE.into());
                        builder.push_default(brush_style);

                        builder.push_default(GenericFamily::SystemUi);
                        builder.push_default(LineHeight::FontSizeRelative(1.3));
                        builder.push_default(StyleProperty::FontSize(16.0));

                        let mut layout = builder.build(text);

                        #[allow(clippy::cast_possible_truncation)]
                        let max_advance = Some(viewport_info.point_size.1 as f32);
                        layout.break_all_lines(max_advance);
                        layout.align(max_advance, Alignment::Start, AlignmentOptions::default());

                        layout
                    },
                    |width, height| {
                        //debug!(width, height);
                        Affine::translate((-width / 2.0, -height / 2.0))
                            .then_rotate(f64::consts::FRAC_PI_2)
                            .then_translate(Vec2::new(height / 2.0, width / 2.0))
                    },
                )
            },
        );

        for event in ctx.events {
            match event {
                InputEvent::PointerEnter { x, y } | InputEvent::PointerMove { time: _, x, y } => {
                    self.pointer = Some((*x, *y));
                },
                InputEvent::PointerLeave => {
                    self.pointer = None;
                },
                _ => {},
            }
        }

        let is_hovering = self.pointer.is_some_and(|p| text_bounds.contains(p));
        if is_hovering != self.hovering {
            ctx.request_redraw();
        }
        self.hovering = is_hovering;

        //ctx.request_redraw();
    }
}
