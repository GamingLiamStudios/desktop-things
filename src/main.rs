#![allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)]
use std::{
    collections::{
        BTreeMap,
        HashMap,
    },
    ptr::NonNull,
    sync::{
        mpsc::channel,
        Arc,
    },
    time::Duration,
};

use egui::{
    mutex::Mutex,
    FullOutput,
    Pos2,
    RawInput,
    Rect,
    RequestRepaintInfo,
    Vec2,
    ViewportBuilder,
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
        PresentMode,
        TextureFormat,
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
            LayerSurface,
        },
        WaylandSurface,
    },
};
use tracing::{
    debug,
    info,
    warn,
};
use tracing_subscriber::{
    filter::Targets,
    fmt,
    layer::SubscriberExt,
    util::SubscriberInitExt,
    Layer,
};
use wayland_client::protocol::{
    wl_output::WlOutput,
    wl_surface::WlSurface,
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

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let stdout_log = fmt::layer();

    tracing_subscriber::registry()
        .with(
            stdout_log.with_filter(
                Targets::default()
                    .with_target("desktop_things", tracing::Level::TRACE)
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

    let (render_state, surface, wgpu_config) = smol::block_on(async {
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

    let render_state = Arc::new(render_state);
    let input = Arc::new(Mutex::new(RawInput::default()));
    let context = egui::Context::default();
    context.set_os(egui::os::OperatingSystem::Nix);
    context.set_embed_viewports(false);

    let mut taskbar = Taskbar {
        registry: RegistryState::new(&globals),
        seat:     SeatState::new(&globals, &qh),
        output:   OutputState::new(&globals, &qh),

        context:      context.clone(),
        input:        input.clone(),
        render_state: render_state.clone(),

        viewports: Arc::new(Mutex::new(HashMap::new())),
        surfaces:  HashMap::new(),
    };

    let root_taskbar_viewport = ViewportId::from_hash_of("source");
    taskbar
        .viewports
        .lock()
        .insert(root_taskbar_viewport, Viewport {
            parent: taskbar.output.outputs().next().expect("no outputs"),
            surface,
            size: (WIDTH, 0),
        });
    taskbar
        .surfaces
        .insert(layer.wl_surface().clone(), root_taskbar_viewport);

    context.show_viewport_deferred(
        root_taskbar_viewport,
        ViewportBuilder::default(),
        |ctx, _| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.label("Hello World!");
            });
        },
    );

    let (request_send, request_recv) = channel();

    context.set_request_repaint_callback(move |repaint_req| {
        let _ = request_send.send(repaint_req);
    });

    let viewports = taskbar.viewports.clone();
    std::thread::spawn(move || {
        while let Ok(RequestRepaintInfo {
            viewport_id,
            delay,
            current_cumulative_pass_nr: _,
        }) = request_recv.recv()
        {
            std::thread::sleep(delay); // TODO: Nicer delay

            // TODO: Build viewport if doesn't exist
            info!("Frame");

            // Grab requested viewport
            let Some(callback) = context.viewport_for(viewport_id, |viewport_state| {
                viewport_state.viewport_ui_cb.clone()
            }) else {
                debug!("Immediate viewport requested repaint");
                return;
            };

            // Get input since last redraw
            let mut input = input.lock().take();

            let viewports = viewports.lock();
            let Some(viewport) = viewports.get(&viewport_id) else {
                warn!("Viewport missing");
                return;
            };

            // Update usable area
            let (width, height) = viewport.size;

            let scale_factor = input
                .viewports
                .entry(viewport_id)
                .or_default()
                .native_pixels_per_point
                .unwrap_or(1.0);

            let pixels_per_point = context.zoom_factor() * scale_factor;
            input.screen_rect = (width > 0 && height > 0).then(|| {
                Rect::from_min_size(
                    Pos2::ZERO,
                    Vec2::new(
                        width as f32 / pixels_per_point,
                        height as f32 / pixels_per_point,
                    ),
                )
            });

            input.viewport_id = viewport_id;

            let FullOutput {
                platform_output: _,
                textures_delta,
                shapes,
                pixels_per_point,
                viewport_output: _,
            } = context.run(input, callback.as_ref());

            let prims = context.tessellate(shapes, pixels_per_point);

            let surface_texture = match viewport.surface.get_current_texture() {
                Ok(frame) => frame,
                Err(err) => match (*wgpu_config.on_surface_error)(err) {
                    SurfaceErrorAction::RecreateSurface => {
                        viewport.configure_surface(
                            &render_state.adapter,
                            &render_state.device,
                            render_state.target_format,
                            PresentMode::Mailbox,
                        );
                        info!("Recreated Surface");
                        return;
                    },
                    SurfaceErrorAction::SkipFrame => {
                        info!("Skipped Frame");
                        return;
                    },
                },
            };
            let surface_view = surface_texture
                .texture
                .create_view(&wgpu::TextureViewDescriptor::default());

            let screen_desc = ScreenDescriptor {
                size_in_pixels: viewport.size.into(),
                pixels_per_point,
            };
            drop(viewports);

            let mut encoder = render_state
                .device
                .create_command_encoder(&CommandEncoderDescriptor::default());

            let buffer_commands = {
                let mut renderer = render_state.renderer.write();

                for (id, image_delta) in textures_delta.set {
                    renderer.update_texture(
                        &render_state.device,
                        &render_state.queue,
                        id,
                        &image_delta,
                    );
                }

                renderer.update_buffers(
                    &render_state.device,
                    &render_state.queue,
                    &mut encoder,
                    &prims,
                    &screen_desc,
                )
            };

            {
                let renderer = render_state.renderer.read();

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
            render_state
                .queue
                .submit(buffer_commands.into_iter().chain([encoder.finish()]));
            surface_texture.present();

            {
                let mut renderer = render_state.renderer.write();
                for id in textures_delta.free {
                    renderer.free_texture(&id);
                }
            }
        }
    });

    loop {
        event_queue
            .blocking_dispatch(&mut taskbar)
            .expect("shitface");
    }
}

struct Viewport {
    parent:  WlOutput,
    surface: wgpu::Surface<'static>,
    size:    (u32, u32),
}

impl Viewport {
    fn configure_surface(
        &self,
        adapter: &wgpu::Adapter,
        device: &wgpu::Device,
        target_format: TextureFormat,
        present_mode: PresentMode,
    ) {
        let (width, height) = self.size;

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: target_format,
            present_mode,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![target_format],
            ..self
                .surface
                .get_default_config(adapter, width, height)
                .expect("The surface isn't supported by this adapter")
        };
        self.surface.configure(device, &surface_config);
    }
}

struct Taskbar {
    render_state: Arc<RenderState>,
    context:      egui::Context,
    input:        Arc<Mutex<RawInput>>,

    viewports: Arc<Mutex<HashMap<ViewportId, Viewport>>>,
    surfaces:  HashMap<WlSurface, ViewportId>,

    registry: RegistryState,
    seat:     SeatState,
    output:   OutputState,
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

impl Dispatch<WpFractionalScaleV1, WlOutput> for Taskbar {
    fn event(
        state: &mut Self,
        _proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        output: &WlOutput,
        _conn: &Connection,
        _qhandle: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
    ) {
        match event {
            wp_fractional_scale_v1::Event::PreferredScale { scale } => {
                // Convert scale to fractional (denom of 120)
                let scale = scale as f32 / 120.0;

                for (&id, viewport) in state.viewports.lock().iter() {
                    if viewport.parent != *output {
                        return;
                    }

                    state
                        .input
                        .lock()
                        .viewports
                        .entry(id)
                        .and_modify(|viewport| viewport.native_pixels_per_point = Some(scale));
                }

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

        let Some(&viewport_id) = self.surfaces.get(layer.wl_surface()) else {
            return;
        };
        self.viewports
            .lock()
            .entry(viewport_id)
            .and_modify(|viewport| {
                let first_draw = viewport.size.1 == 0;

                viewport.size = configure.new_size;
                viewport.configure_surface(
                    &self.render_state.adapter,
                    &self.render_state.device,
                    self.render_state.target_format,
                    PresentMode::Mailbox,
                );

                if first_draw {
                    self.context.request_repaint_of(viewport_id);
                }
            });

        /*
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
        */
    }
}

impl CompositorHandler for Taskbar {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        surface: &smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let Some(&id) = self.surfaces.get(surface) else {
            return;
        };

        self.input
            .lock()
            .viewports
            .entry(id)
            .and_modify(|viewport| {
                if viewport
                    .native_pixels_per_point
                    .unwrap_or(1.0)
                    .fract()
                    .abs()
                    <= f32::EPSILON
                {
                    viewport.native_pixels_per_point = Some(new_factor as f32);

                    debug!(scale = new_factor, "Updated Integer Scale");
                }
            });
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
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        surface: &smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface,
        time: u32,
    ) {
        self.input.lock().time = Some(Duration::from_millis(u64::from(time)).as_secs_f64());
        if let Some(&id) = self.surfaces.get(surface) {
            self.context.request_repaint_of(id);
        }
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
