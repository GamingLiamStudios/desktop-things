#![allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)]
use std::{
    ptr::NonNull,
    time::Duration,
};

use egui::{
    Pos2,
    Rect,
    Vec2,
    ViewportId,
};
use egui_wgpu::{
    wgpu::{
        self,
        rwh::{
            RawDisplayHandle,
            RawWindowHandle,
            WaylandDisplayHandle,
            WaylandWindowHandle,
        },
        CommandEncoderDescriptor,
    },
    RenderState,
    ScreenDescriptor,
    SurfaceErrorAction,
    WgpuConfiguration,
};
use river_status_unstable_v1::zriver_status_manager_v1::{
    self,
    ZriverStatusManagerV1,
};
use smithay_client_toolkit::{
    compositor::{
        CompositorHandler,
        CompositorState,
    },
    delegate_compositor,
    delegate_layer,
    delegate_output,
    delegate_registry,
    delegate_seat,
    globals::GlobalData,
    output::{
        OutputHandler,
        OutputState,
    },
    reexports::{
        client::{
            globals::registry_queue_init,
            Connection,
            Dispatch,
            Proxy,
        },
        protocols::wp::fractional_scale::v1::client::{
            wp_fractional_scale_manager_v1::{
                self,
                WpFractionalScaleManagerV1,
            },
            wp_fractional_scale_v1::{
                self,
                WpFractionalScaleV1,
            },
        },
    },
    registry::{
        ProvidesRegistryState,
        RegistryState,
    },
    registry_handlers,
    seat::{
        SeatHandler,
        SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor,
            LayerShell,
            LayerShellHandler,
        },
        WaylandSurface,
    },
};
use tracing::{
    debug,
    info,
};
use tracing_subscriber::{
    filter::Targets,
    fmt,
    layer::SubscriberExt,
    util::SubscriberInitExt,
    Layer,
};

const WIDTH: u32 = 60;

#[allow(clippy::wildcard_imports)]
pub mod river_control_unstable_v1 {
    use wayland_client::{
        self,
        protocol::*,
    };

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/river-control-unstable-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/river-control-unstable-v1.xml");
}

#[allow(clippy::wildcard_imports)]
pub mod river_status_unstable_v1 {
    use wayland_client::{
        self,
        protocol::*,
    };

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/river-status-unstable-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/river-status-unstable-v1.xml");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let stdout_log = fmt::layer();

    tracing_subscriber::registry()
        .with(
            stdout_log.with_filter(
                Targets::default()
                    .with_target("desktop-things", tracing::Level::TRACE)
                    .with_target("wgpu", tracing::Level::WARN)
                    .with_target("egui", tracing::Level::WARN)
                    .with_target("eframe", tracing::Level::WARN)
                    .with_default(tracing::Level::INFO),
            ),
        )
        .init();

    let wayland_conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&wayland_conn)?;
    let qh = event_queue.handle();

    let fractional_manager: WpFractionalScaleManagerV1 = globals.bind(&qh, 1..=1, GlobalData)?;
    let river_status_manager: ZriverStatusManagerV1 = globals.bind(&qh, 4..=4, GlobalData)?;

    // Before we can use river status thing, we need to know the current output
    // however that looks to be very annoying, and honestly the best way looks to
    // be simply just letting a different taskbar be rendered per screen attached.
    // Then we can simply just manually specify the output for the bar and all's
    // good with the world.

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let surface = compositor.create_surface(&qh);

    let _ = fractional_manager.get_fractional_scale(&surface, &qh, GlobalData);

    let display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
        NonNull::new(wayland_conn.backend().display_ptr().cast()).expect("shitface"),
    ));
    let surface_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
        NonNull::new(surface.id().as_ptr().cast()).expect("shitface"),
    ));

    let layers = LayerShell::bind(&globals, &qh)?;
    let layer = layers.create_layer_surface(
        &qh,
        surface,
        smithay_client_toolkit::shell::wlr_layer::Layer::Top,
        Some("desktop-things"),
        None,
    );

    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT);
    layer.set_exclusive_zone(WIDTH as i32);
    layer.set_size(WIDTH, 0);
    layer.commit();

    let (state, surface, wgpu_config) = smol::block_on(async {
        let setup = egui_wgpu::WgpuSetupCreateNew {
            power_preference: egui_wgpu::wgpu::PowerPreference::LowPower,
            ..Default::default()
        };

        let instance = egui_wgpu::WgpuSetup::CreateNew(setup.clone())
            .new_instance()
            .await;
        let surface = unsafe {
            instance.create_surface_unsafe(egui_wgpu::wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: display_handle,
                raw_window_handle:  surface_handle,
            })?
        };

        let wgpu_config = egui_wgpu::WgpuConfiguration {
            wgpu_setup: setup.into(),
            ..Default::default()
        };

        let state =
            egui_wgpu::RenderState::create(&wgpu_config, &instance, Some(&surface), None, 1, false)
                .await?;

        Ok::<_, Box<dyn std::error::Error>>((state, surface, wgpu_config))
    })?;

    let mut taskbar = Taskbar {
        registry: RegistryState::new(&globals),
        seat: SeatState::new(&globals, &qh),
        output: OutputState::new(&globals, &qh),

        context: egui::Context::default(),
        input: egui::RawInput::default(),
        wgpu_config,

        render: state,

        surface,
        size: (WIDTH, 0),
    };

    loop {
        event_queue
            .blocking_dispatch(&mut taskbar)
            .expect("shitface");
    }
}

struct Taskbar {
    context:     egui::Context,
    input:       egui::RawInput,
    wgpu_config: WgpuConfiguration,

    render:  RenderState,
    surface: wgpu::Surface<'static>,
    size:    (u32, u32),

    registry: RegistryState,
    seat:     SeatState,
    output:   OutputState,
}

impl Taskbar {
    fn configure_surface(&self) {
        let (width, height) = self.size;

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.render.target_format,
            present_mode: self.wgpu_config.present_mode,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![self.render.target_format],
            ..self
                .surface
                .get_default_config(&self.render.adapter, width, height)
                .expect("The surface isn't supported by this adapter")
        };
        self.surface.configure(&self.render.device, &surface_config);
    }

    #[allow(clippy::unused_self)]
    fn render(
        &self,
        ctx: &egui::Context,
    ) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.label("hello");
        });
    }

    fn draw(&mut self) {
        // Render egui shit
        let scale_factor = self
            .input
            .viewports
            .entry(ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point
            .unwrap_or(1.0);
        let pixels_per_point = self.context.zoom_factor() * scale_factor;
        self.input.screen_rect = (self.size.0 > 0 && self.size.1 > 0).then(|| {
            Rect::from_min_size(
                Pos2::ZERO,
                Vec2::new(
                    self.size.0 as f32 / pixels_per_point,
                    self.size.1 as f32 / pixels_per_point,
                ),
            )
        });
        let input = self.input.take();

        let output = self.context.run(input, |ctx| self.render(ctx));
        let prims = self
            .context
            .tessellate(output.shapes, output.pixels_per_point);

        let surface_texture = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(err) => match (*self.wgpu_config.on_surface_error)(err) {
                SurfaceErrorAction::RecreateSurface => {
                    self.configure_surface();
                    return;
                },
                SurfaceErrorAction::SkipFrame => {
                    return;
                },
            },
        };
        let surface_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let screen_desc = ScreenDescriptor {
            size_in_pixels:   [
                surface_texture.texture.width(),
                surface_texture.texture.height(),
            ],
            pixels_per_point: output.pixels_per_point,
        };

        let mut encoder = self
            .render
            .device
            .create_command_encoder(&CommandEncoderDescriptor::default());

        let buffer_commands = {
            let mut renderer = self.render.renderer.write();

            for (id, image_delta) in output.textures_delta.set {
                renderer.update_texture(&self.render.device, &self.render.queue, id, &image_delta);
            }

            renderer.update_buffers(
                &self.render.device,
                &self.render.queue,
                &mut encoder,
                &prims,
                &screen_desc,
            )
        };

        {
            let renderer = self.render.renderer.read();

            let render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label:                    None,
                color_attachments:        &[Some(wgpu::RenderPassColorAttachment {
                    view:           &surface_view,
                    resolve_target: None,
                    ops:            wgpu::Operations {
                        load:  wgpu::LoadOp::Clear(wgpu::Color::BLUE),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes:         None,
                occlusion_query_set:      None,
            });

            renderer.render(&mut render_pass.forget_lifetime(), &prims, &screen_desc);
        }

        // Submit the command in the queue to execute
        self.render
            .queue
            .submit(buffer_commands.into_iter().chain([encoder.finish()]));
        surface_texture.present();

        {
            let mut renderer = self.render.renderer.write();
            for id in &output.textures_delta.free {
                renderer.free_texture(id);
            }
        }
    }
}

impl Dispatch<WpFractionalScaleManagerV1, GlobalData> for Taskbar {
    fn event(
        _state: &mut Self,
        _proxy: &WpFractionalScaleManagerV1,
        _event: wp_fractional_scale_manager_v1::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
    ) {
        unimplemented!("There are no events for FractionalScaleManager")
    }
}

impl Dispatch<WpFractionalScaleV1, GlobalData> for Taskbar {
    fn event(
        state: &mut Self,
        _proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
    ) {
        match event {
            wp_fractional_scale_v1::Event::PreferredScale { scale } => {
                // Convert scale to fractional (denom of 120)
                let scale = scale as f32 / 120.0;
                state
                    .input
                    .viewports
                    .entry(ViewportId::ROOT)
                    .or_default()
                    .native_pixels_per_point = Some(scale);
                debug!(scale, "Updated Fractional Scale");
            },
            _ => unreachable!("There shouldn't be any other events"),
        }
    }
}

impl Dispatch<ZriverStatusManagerV1, GlobalData> for Taskbar {
    fn event(
        _state: &mut Self,
        _proxy: &ZriverStatusManagerV1,
        _event: zriver_status_manager_v1::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
    ) {
        unimplemented!("There are no events for RiverStatusManager")
    }
}

impl LayerShellHandler for Taskbar {
    fn closed(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _layer: &smithay_client_toolkit::shell::wlr_layer::LayerSurface,
    ) {
        // Unused
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        layer: &smithay_client_toolkit::shell::wlr_layer::LayerSurface,
        configure: smithay_client_toolkit::shell::wlr_layer::LayerSurfaceConfigure,
        _serial: u32,
    ) {
        info!(new_size = ?configure.new_size, "Configuring WGpu instance with updated size");
        let first_draw = self.size.1 == 0;
        self.size = configure.new_size;
        self.configure_surface();

        if first_draw {
            self.draw();

            // Queue next frame
            layer
                .wl_surface()
                .damage_buffer(0, 0, self.size.0 as i32, self.size.1 as i32);
            layer.wl_surface().frame(qh, layer.wl_surface().clone());
            layer.commit();
        }
    }
}

impl CompositorHandler for Taskbar {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _surface: &smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface,
        new_factor: i32,
    ) {
        if let Some(ppp) = self
            .input
            .viewports
            .entry(ViewportId::ROOT)
            .or_default()
            .native_pixels_per_point
        {
            if ppp.fract().abs() <= f32::EPSILON {
                self.input
                    .viewports
                    .entry(ViewportId::ROOT)
                    .or_default()
                    .native_pixels_per_point = Some(new_factor as f32);
                debug!(scale = new_factor, "Updated Integer Scale");
            }
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _surface: &smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface,
        _new_transform: smithay_client_toolkit::reexports::client::protocol::wl_output::Transform,
    ) {
        // Unused
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        surface: &smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface,
        time: u32,
    ) {
        self.input.time = Some(Duration::from_millis(u64::from(time)).as_secs_f64());
        self.draw();

        surface.damage_buffer(0, 0, self.size.0 as i32, self.size.1 as i32);
        surface.frame(qh, surface.clone());
        surface.commit();
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _surface: &smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface,
        _output: &smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput,
    ) {
        // Unused
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _surface: &smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface,
        _output: &smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput,
    ) {
        // Unused
    }
}

impl OutputHandler for Taskbar {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _output: smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput,
    ) {
        // Unused
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _output: smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput,
    ) {
        // Unused
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _output: smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput,
    ) {
        // Unused
    }
}

impl SeatHandler for Taskbar {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }

    fn new_seat(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _seat: smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat,
    ) {
        // Unused
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _seat: smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat,
        _capability: smithay_client_toolkit::seat::Capability,
    ) {
        // Unused
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _seat: smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat,
        _capability: smithay_client_toolkit::seat::Capability,
    ) {
        // Unused
    }

    fn remove_seat(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _seat: smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat,
    ) {
        // Unused
    }
}

delegate_registry!(Taskbar);
delegate_seat!(Taskbar);
delegate_output!(Taskbar);
delegate_compositor!(Taskbar);
delegate_layer!(Taskbar);

impl ProvidesRegistryState for Taskbar {
    registry_handlers![OutputState, SeatState,];

    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
}
