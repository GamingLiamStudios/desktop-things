use std::{
    fmt::Debug,
    time::Duration,
};

use parley::{
    FontContext,
    Layout,
    LayoutContext,
    PositionedInlineBox,
};
use vello::{
    kurbo::{
        Affine,
        Line,
        Point,
        Rect,
        Size,
        Stroke,
    },
    peniko::{
        self,
        Brush,
        BrushRef,
        Color,
        Fill,
    },
};

pub mod layershell;

pub struct RenderContext<'a> {
    scene:  vello::Scene,
    events: Vec<InputEvent>,

    pub viewport_info: ViewportInfo,

    font_context:   &'a mut FontContext,
    layout_context: &'a mut LayoutContext<Brush>,

    pub current_time: Duration,
    requested_redraw: Option<Duration>,
}

impl RenderContext<'_> {
    pub const fn request_redraw(&mut self) {
        self.requested_redraw = Some(Duration::ZERO);
    }
}

#[derive(Clone, Copy)]
pub struct ViewportInfo {
    pub window_size: (u32, u32),
    pub pixel_scale: f32,
}

impl RenderContext<'_> {
    pub fn add_text(
        &mut self,
        text: impl Fn(
            &mut FontContext,
            &mut LayoutContext<Brush>,
            &ViewportInfo,
        ) -> Layout<peniko::Brush>,
        transform: impl Fn(f32, f32) -> Affine,
    ) {
        let layout = text(self.font_context, self.layout_context, &self.viewport_info);

        let transform = transform(layout.width(), layout.height());

        // Shamelessly stolen from parley examples
        for line in layout.lines() {
            for item in line.items() {
                match item {
                    parley::PositionedLayoutItem::GlyphRun(glyph) => {
                        let style = glyph.style();

                        // We draw underlines under the text, then the strikethrough on top,
                        // following: https://drafts.csswg.org/css-text-decor/#painting-order
                        if let Some(underline) = &style.underline {
                            let run_metrics = glyph.run().metrics();
                            let offset = underline
                                .offset
                                .map_or(run_metrics.underline_offset, |offset| offset);
                            let width = underline
                                .size
                                .map_or(run_metrics.underline_size, |size| size);

                            // The `offset` is the distance from the baseline to the top of the
                            // underline so we move the line down by
                            // half the width Remember that we are using
                            // a y-down coordinate system If there's a
                            // custom width, because this is an underline, we want the custom
                            // width to go down from the default expectation
                            let y = glyph.baseline() - offset + width / 2.;

                            let line = Line::new(
                                (f64::from(glyph.offset()), f64::from(y)),
                                (f64::from(glyph.offset() + glyph.advance()), f64::from(y)),
                            );
                            self.scene.stroke(
                                &Stroke::new(width.into()),
                                transform,
                                &style.brush,
                                None,
                                &line,
                            );
                        }

                        let mut x = glyph.offset();
                        let y = glyph.baseline();
                        let run = glyph.run();
                        let font = run.font();
                        let font_size = run.font_size();
                        let synthesis = run.synthesis();
                        let glyph_xform = synthesis
                            .skew()
                            .map(|angle| Affine::skew(f64::from(angle.to_radians().tan()), 0.0));

                        self.scene
                            .draw_glyphs(font)
                            .brush(&style.brush)
                            .hint(true)
                            .transform(transform)
                            .glyph_transform(glyph_xform)
                            .font_size(font_size)
                            .normalized_coords(run.normalized_coords())
                            .draw(
                                Fill::NonZero,
                                glyph.glyphs().map(|glyph| {
                                    let gx = x + glyph.x;
                                    let gy = y - glyph.y;
                                    x += glyph.advance;
                                    vello::Glyph {
                                        id: glyph.id,
                                        x:  gx,
                                        y:  gy,
                                    }
                                }),
                            );

                        if let Some(strikethrough) = &style.strikethrough {
                            let run_metrics = glyph.run().metrics();
                            let offset = strikethrough
                                .offset
                                .map_or(run_metrics.strikethrough_offset, |offset| offset);
                            let width = strikethrough
                                .size
                                .map_or(run_metrics.strikethrough_size, |size| size);

                            // The `offset` is the distance from the baseline to the *top* of the
                            // strikethrough so we calculate the middle
                            // y-position of the strikethrough based on the font's
                            // standard strikethrough width.
                            // Remember that we are using a y-down coordinate system
                            let y = glyph.baseline() - offset + run_metrics.strikethrough_size / 2.;

                            let line = Line::new(
                                (f64::from(glyph.offset()), f64::from(y)),
                                (f64::from(glyph.offset() + glyph.advance()), f64::from(y)),
                            );
                            self.scene.stroke(
                                &Stroke::new(width.into()),
                                transform,
                                &style.brush,
                                None,
                                &line,
                            );
                        }
                    },
                    parley::PositionedLayoutItem::InlineBox(PositionedInlineBox {
                        x,
                        y,
                        width,
                        height,
                        id: _,
                    }) => {
                        let origin = Point::new(f64::from(x), f64::from(y));
                        let size = Size::new(f64::from(x + width), f64::from(y + height));

                        self.scene.fill(
                            vello::peniko::Fill::NonZero,
                            transform,
                            Color::WHITE,
                            None,
                            &Rect::from_origin_size(origin, size),
                        );
                    },
                }
            }
        }
    }
}

pub enum InputEvent {
    PointerEnter {
        x: f64,
        y: f64,
    },
    PointerMove {
        time: Duration,
        x:    f64,
        y:    f64,
    },
    PointerButton {
        time:   Duration,
        button: u32,
        state:  bool,
    },
    PointerAxis {
        time:       Duration,
        horizontal: f64,
        vertical:   f64,
    },
    PointerLeave,
    KeyboardKey,
}

pub trait Program {
    fn draw(
        &mut self,
        ctx: &mut RenderContext,
    );
}
