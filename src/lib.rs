use std::{
    fmt::Debug,
    time::Duration,
};

use parley::{
    FontContext,
    GlyphRun,
    Layout,
    LayoutContext,
    PositionedInlineBox,
};
use vello::{
    Scene,
    kurbo::{
        Affine,
        Line,
        Point,
        Rect,
        RoundedRectRadii,
        Shape,
        Size,
        Stroke,
        Vec2,
    },
    peniko::{
        self,
        Brush,
        Color,
        Fill,
    },
};

pub mod layershell;

pub struct RenderContext<'a> {
    scene:      vello::Scene,
    pub events: &'a [InputEvent],

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
    pub point_size:  (f64, f64),
    pub pixel_scale: f64,
}

impl ViewportInfo {
    #[must_use]
    pub fn from_pixel_size(
        width: u32,
        height: u32,
        scale: f64,
    ) -> Self {
        let recip_scale = scale.recip();

        Self {
            point_size:  (
                f64::from(width) * recip_scale,
                f64::from(height) * recip_scale,
            ),
            pixel_scale: scale,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FrameEdge {
    color:  Color,
    stroke: Stroke,
}

#[derive(Debug, Clone)]
pub struct FrameInfo {
    margin:  Option<(f64, f64)>,
    padding: Option<(f64, f64)>,
    radius:  Option<f64>,

    edge:  Option<FrameEdge>,
    color: Color,
}

impl FrameInfo {
    #[must_use]
    pub const fn new(color: Color) -> Self {
        Self {
            color,

            radius: None,
            margin: None,
            padding: None,
            edge: None,
        }
    }

    #[must_use]
    pub fn with_radius(
        self,
        radius: f64,
    ) -> Self {
        Self {
            radius: Some(radius),
            ..self
        }
    }

    #[must_use]
    pub fn with_margin(
        self,
        size: (f64, f64),
    ) -> Self {
        Self {
            margin: Some(size),
            ..self
        }
    }

    #[must_use]
    pub fn with_padding(
        self,
        size: (f64, f64),
    ) -> Self {
        Self {
            padding: Some(size),
            ..self
        }
    }

    #[must_use]
    pub fn with_border(
        self,
        stroke: Stroke,
        color: Color,
    ) -> Self {
        Self {
            edge: Some(FrameEdge { color, stroke }),
            ..self
        }
    }
}

impl RenderContext<'_> {
    pub fn frame(
        &mut self,
        frame: &FrameInfo,
        transform: impl Fn(f64, f64) -> Affine,
        inner: impl Fn(&mut RenderContext) -> Rect,
    ) -> Rect {
        let mut scene = vello::Scene::new();

        let (width, height) = self.viewport_info.point_size;
        let (margin_w, margin_h) = frame.margin.unwrap_or((0.0, 0.0));
        let (padding_w, padding_h) = frame.padding.unwrap_or((0.0, 0.0));

        let full_content_rect = Rect::from_origin_size(
            Point::ZERO,
            Size::new(
                (margin_w + padding_w).mul_add(-2.0, width),
                (margin_h + padding_h).mul_add(-2.0, height),
            ),
        );
        scene.push_clip_layer(Affine::IDENTITY, &full_content_rect);

        let mut ctx = RenderContext {
            scene,
            events: self.events,
            viewport_info: ViewportInfo {
                point_size:  (full_content_rect.width(), full_content_rect.height()),
                pixel_scale: self.viewport_info.pixel_scale,
            },
            font_context: self.font_context,
            layout_context: self.layout_context,
            current_time: self.current_time,
            requested_redraw: self.requested_redraw,
        };

        let bounds = inner(&mut ctx).intersect(full_content_rect);
        let transform = transform(bounds.width(), bounds.height());

        let bounds = transform.transform_rect_bbox(bounds);
        ctx.scene.pop_layer();

        let frame_rect = Rect::from_origin_size(
            Point::new(margin_w, margin_h),
            Size::new(
                padding_w.mul_add(2.0, bounds.width()) + margin_w,
                padding_h.mul_add(2.0, bounds.height()) + margin_h,
            ),
        )
        .to_rounded_rect(RoundedRectRadii::from_single_radius(
            frame.radius.unwrap_or(0.0),
        ));

        self.scene.fill(
            Fill::NonZero,
            Affine::IDENTITY,
            frame.color,
            None,
            &frame_rect,
        );
        self.scene.append(
            &ctx.scene,
            Some(transform.then_translate(Vec2::new(margin_w + padding_w, margin_h + padding_h))),
        );

        if let Some(FrameEdge { color, stroke }) = &frame.edge {
            self.scene
                .stroke(stroke, Affine::IDENTITY, color, None, &frame_rect);
        }

        if let Some(duration) = ctx.requested_redraw {
            self.requested_redraw = Some(self.requested_redraw.map_or(duration, |other| {
                if duration > other { other } else { duration }
            }));
        }

        frame_rect.bounding_box()
    }

    fn draw_glyph(
        scene: &mut Scene,
        glyph: &GlyphRun<'_, Brush>,
    ) {
        // Shamelessly stolen from parley examples
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
            scene.stroke(
                &Stroke::new(width.into()),
                Affine::IDENTITY,
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

        scene
            .draw_glyphs(font)
            .brush(&style.brush)
            .hint(true)
            .transform(Affine::IDENTITY)
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
            scene.stroke(
                &Stroke::new(width.into()),
                Affine::IDENTITY,
                &style.brush,
                None,
                &line,
            );
        }
    }

    pub fn add_text(
        &mut self,
        text: impl Fn(
            &mut FontContext,
            &mut LayoutContext<Brush>,
            &ViewportInfo,
        ) -> Layout<peniko::Brush>,
        transform: impl Fn(f64, f64) -> Affine,
    ) -> Rect {
        let layout = text(self.font_context, self.layout_context, &self.viewport_info);

        let transform = transform(f64::from(layout.width()), f64::from(layout.height()));
        let clip_rect = Rect::from_origin_size(
            Point::ZERO,
            (f64::from(layout.width()), f64::from(layout.height())),
        );

        let mut scene = Scene::new();
        scene.push_clip_layer(Affine::IDENTITY, &clip_rect);
        for line in layout.lines() {
            for item in line.items() {
                match item {
                    parley::PositionedLayoutItem::GlyphRun(glyph) => {
                        Self::draw_glyph(&mut scene, &glyph);
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

                        scene.fill(
                            vello::peniko::Fill::NonZero,
                            Affine::IDENTITY,
                            Color::WHITE,
                            None,
                            &Rect::from_origin_size(origin, size),
                        );
                    },
                }
            }
        }

        scene.pop_layer();
        self.scene.append(&scene, Some(transform));

        transform
            .transform_rect_bbox(clip_rect)
            .with_origin(Point::ZERO) // Removes floating-point error on origin after translation
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
