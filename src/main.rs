#![allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)]
use std::{
    collections::HashMap,
    f32::consts::PI,
    io::ErrorKind,
    os::{
        fd::{
            AsFd,
            AsRawFd,
            FromRawFd,
            RawFd,
        },
        unix::net::UnixStream,
    },
    ptr::NonNull,
    sync::{
        mpsc::channel,
        Arc,
    },
    task,
    time::Duration,
};

use egui::{
    emath::Rot2,
    epaint::TextShape,
    mutex::Mutex,
    text::{
        LayoutJob,
        TextWrapping,
    },
    Color32,
    CornerRadius,
    FontSelection,
    FullOutput,
    Layout,
    Margin,
    Modifiers,
    PointerButton,
    Pos2,
    RawInput,
    Rect,
    RequestRepaintInfo,
    RichText,
    Sense,
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
};
use futures_lite::FutureExt;
use river_status_unstable_v1::{
    zriver_output_status_v1::{
        self,
        ZriverOutputStatusV1,
    },
    zriver_seat_status_v1::{
        self,
        ZriverSeatStatusV1,
    },
    zriver_status_manager_v1::{
        self,
        ZriverStatusManagerV1,
    },
};
use smithay_client_toolkit::{
    compositor::{
        CompositorHandler,
        CompositorState,
    },
    delegate_compositor,
    delegate_layer,
    delegate_output,
    delegate_pointer,
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
        pointer::{
            PointerData,
            PointerEvent,
            PointerHandler,
            BTN_EXTRA,
            BTN_LEFT,
            BTN_MIDDLE,
            BTN_RIGHT,
            BTN_SIDE,
        },
        Capability,
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
use smol::{
    Async,
    Executor,
    LocalExecutor,
};
use tracing::{
    debug,
    info,
    trace,
    warn,
};
use tracing_subscriber::{
    filter::Targets,
    fmt,
    layer::SubscriberExt,
    util::SubscriberInitExt,
    Layer,
};
use wayland_backend::client::WaylandError;
use wayland_client::{
    protocol::{
        wl_output::WlOutput,
        wl_seat::WlSeat,
        wl_surface::WlSurface,
    },
    DispatchError,
};

const WIDTH: u32 = 40;

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
                    .with_target("desktop_things", tracing::Level::DEBUG)
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

    // Before we can use river status thing, we need to know the current output
    // however that looks to be very annoying, and honestly the best way looks to
    // be simply just letting a different taskbar be rendered per screen attached.
    // Then we can simply just manually specify the output for the bar and all's
    // good with the world.

    smol::block_on(async {
        let setup = egui_wgpu::WgpuSetupCreateNew {
            power_preference: egui_wgpu::wgpu::PowerPreference::LowPower,
            ..Default::default()
        };

        let instance = egui_wgpu::WgpuSetup::CreateNew(setup.clone())
            .new_instance()
            .await;

        let wgpu_config = egui_wgpu::WgpuConfiguration {
            wgpu_setup: setup.into(),
            ..Default::default()
        };

        let render_state =
            egui_wgpu::RenderState::create(&wgpu_config, &instance, None, None, 1, false).await?;

        let render_state = Arc::new(render_state);
        let input = Arc::new(Mutex::new(RawInput::default()));
        let context = egui::Context::default();
        context.set_os(egui::os::OperatingSystem::Nix);
        context.set_embed_viewports(false);

        let mut taskbar = Taskbar {
            registry: RegistryState::new(&globals),
            seat: SeatState::new(&globals, &qh),
            output: OutputState::new(&globals, &qh),

            compositor: CompositorState::bind(&globals, &qh)?,
            layers: LayerShell::bind(&globals, &qh)?,
            fractional: globals.bind(&qh, 1..=1, GlobalData)?,

            river_status: globals.bind(&qh, 4..=4, GlobalData)?,
            river_outputs: HashMap::new(),
            river_focus: HashMap::new(),

            context: context.clone(),
            input: input.clone(),
            render_state: render_state.clone(),
            instance,

            viewports: Arc::new(Mutex::new(HashMap::new())),
            surfaces: HashMap::new(),
        };

        event_queue.roundtrip(&mut taskbar)?;

        let exec = Arc::new(Executor::new());
        let viewports = taskbar.viewports.clone();

        let executor = exec.clone();
        taskbar.context.set_request_repaint_callback(
            move |RequestRepaintInfo {
                      viewport_id,
                      delay,
                      current_cumulative_pass_nr: _, // I would use this but it seems to deadlock
                  }| {
                let context = context.clone();
                let input = input.clone();
                let viewports = viewports.clone();
                let render_state = render_state.clone();
                let wgpu_config = wgpu_config.clone();

                exec.spawn(async move {
                    smol::Timer::after(delay).await;

                    // TODO: Build viewport if doesn't exist (tooltips)
                    trace!(id = ?viewport_id, "Rendering viewport frame");

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

                    let screen_desc = ScreenDescriptor {
                        size_in_pixels: viewport.size.into(),
                        pixels_per_point,
                    };

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

                    if width == 0 || height == 0 {
                        warn!("Viewport not configured");

                        render_state.queue.submit(buffer_commands);
                    } else {
                        let surface_texture = match viewport.surface.get_current_texture() {
                            Ok(frame) => frame,
                            Err(err) => match (*wgpu_config.on_surface_error)(err) {
                                SurfaceErrorAction::RecreateSurface => {
                                    trace!("WGpu requested surface reconfiguration");
                                    viewport.configure_surface(
                                        &render_state.adapter,
                                        &render_state.device,
                                        render_state.target_format,
                                        wgpu_config.present_mode,
                                    );
                                    return;
                                },
                                SurfaceErrorAction::SkipFrame => {
                                    trace!("Skipped Frame");
                                    return;
                                },
                            },
                        };
                        let surface_view = surface_texture
                            .texture
                            .create_view(&wgpu::TextureViewDescriptor::default());

                        let render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label:                    None,
                            color_attachments:        &[Some(wgpu::RenderPassColorAttachment {
                                view:           &surface_view,
                                resolve_target: None,
                                ops:            wgpu::Operations {
                                    load:  wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                                    store: wgpu::StoreOp::Store,
                                },
                            })],
                            depth_stencil_attachment: None,
                            timestamp_writes:         None,
                            occlusion_query_set:      None,
                        });

                        render_state.renderer.read().render(
                            &mut render_pass.forget_lifetime(),
                            &prims,
                            &screen_desc,
                        );

                        // Submit the command in the queue to execute
                        render_state
                            .queue
                            .submit(buffer_commands.into_iter().chain([encoder.finish()]));
                        surface_texture.present();
                    }

                    {
                        let mut renderer = render_state.renderer.write();
                        for id in textures_delta.free {
                            renderer.free_texture(&id);
                        }
                    }
                })
                .detach();
            },
        );

        executor
            .run(smol::unblock::<Result<_, DispatchError>, _>(move || loop {
                event_queue.blocking_dispatch(&mut taskbar)?;
            }))
            .await?;

        Ok(())
    })
}

struct Viewport {
    parent: WlOutput,
    layer:  LayerSurface,

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
            alpha_mode: wgpu::CompositeAlphaMode::PreMultiplied,
            view_formats: vec![target_format],
            ..self
                .surface
                .get_default_config(adapter, width, height)
                .expect("The surface isn't supported by this adapter")
        };
        self.surface.configure(device, &surface_config);
    }
}

impl Drop for Viewport {
    fn drop(&mut self) {
        self.layer.wl_surface().destroy();
    }
}

struct RiverOutputStatus {
    used:    u32,
    focused: u32,
    urgent:  u32,

    view_title: String,
}

struct Taskbar {
    instance:     wgpu::Instance,
    render_state: Arc<RenderState>,

    context: egui::Context,
    input:   Arc<Mutex<RawInput>>,

    viewports: Arc<Mutex<HashMap<ViewportId, Viewport>>>,
    surfaces:  HashMap<WlSurface, ViewportId>,

    compositor: CompositorState,
    layers:     LayerShell,
    fractional: WpFractionalScaleManagerV1,

    river_status:  ZriverStatusManagerV1,
    river_outputs: HashMap<WlOutput, Arc<Mutex<RiverOutputStatus>>>,
    river_focus:   HashMap<WlSeat, WlOutput>,

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

                debug!(scale, "Updated Scale Factor (fractional)");
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

impl Dispatch<ZriverOutputStatusV1, WlOutput> for Taskbar {
    fn event(
        state: &mut Self,
        _proxy: &ZriverOutputStatusV1,
        event: zriver_output_status_v1::Event,
        focused: &WlOutput,
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        debug!(?event);

        let Some(status) = state.river_outputs.get_mut(focused) else {
            warn!("River returning info about unknown Output");
            return;
        };
        {
            let mut status = status.lock();

            match event {
                zriver_output_status_v1::Event::FocusedTags { tags } => {
                    // Currently showing
                    status.focused = tags;
                },
                zriver_output_status_v1::Event::UrgentTags { tags } => {
                    // Notifications
                    status.urgent = tags;
                },
                zriver_output_status_v1::Event::ViewTags { tags } => {
                    // List of views & their tags
                    let views: &[u32] = bytemuck::cast_slice(&tags); // TODO: Do nicer things later with this info
                    status.used = views.iter().fold(0u32, |acc, v| acc | v);
                },
                _ => {}, // We don't need the layout info
            }
        }

        for (id, _) in state
            .viewports
            .lock()
            .iter()
            .filter(|(_, viewport)| viewport.parent == *focused)
        {
            state.context.request_repaint_of(*id);
        }
    }
}

impl Dispatch<ZriverSeatStatusV1, WlSeat> for Taskbar {
    #[allow(clippy::significant_drop_tightening)]
    fn event(
        state: &mut Self,
        _proxy: &ZriverSeatStatusV1,
        event: zriver_seat_status_v1::Event,
        seat: &WlSeat,
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            zriver_seat_status_v1::Event::FocusedOutput { output } => {
                state.river_focus.insert(seat.clone(), output);
            },
            zriver_seat_status_v1::Event::UnfocusedOutput { output: _ } => {
                // Assuming seat cannot focus more than one output (for convenience sake)
                state.river_focus.remove(seat);
            },
            zriver_seat_status_v1::Event::FocusedView { title } => {
                let focused = state
                    .river_focus
                    .get(seat)
                    .expect("Seat not attached to Output focused on View");

                let Some(status) = state.river_outputs.get_mut(focused) else {
                    warn!("Seat attached to unknown Output");
                    return;
                };

                status.lock().view_title = title;
                for (id, _) in state
                    .viewports
                    .lock()
                    .iter()
                    .filter(|(_, viewport)| viewport.parent == *focused)
                {
                    state.context.request_repaint_of(*id);
                }
            },
            zriver_seat_status_v1::Event::Mode { name: _ } => {
                // Unused
            },
        }
    }
}

impl LayerShellHandler for Taskbar {
    fn closed(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _layer: &smithay_client_toolkit::shell::wlr_layer::LayerSurface,
    ) {
        // TODO: Handle layer closing
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        layer: &smithay_client_toolkit::shell::wlr_layer::LayerSurface,
        configure: smithay_client_toolkit::shell::wlr_layer::LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(viewport_id) = self.surfaces.get(layer.wl_surface()) else {
            return;
        };

        if let Some(viewport) = self.viewports.lock().get_mut(viewport_id) {
            let first_draw = viewport.size.1 == 0;

            viewport.size = configure.new_size;
            viewport.configure_surface(
                &self.render_state.adapter,
                &self.render_state.device,
                self.render_state.target_format,
                PresentMode::AutoVsync,
            );
            debug!(new_size = ?configure.new_size, "Viewport resized");

            if first_draw {
                self.context.request_repaint_of(*viewport_id);
            }
        }

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

                    debug!(scale = new_factor, "Updated Scale Factor (integer)");
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
        // TODO: Allow rotation of UI to align with orientation of output
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

    #[allow(clippy::too_many_lines)]
    fn new_output(
        &mut self,
        conn: &Connection,
        qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        output: smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput,
    ) {
        // Spawn new Taskbar on output
        debug!(id = ?output.id(), "Initalizing taskbar frame for new output");
        let surface = self.compositor.create_surface(qh);

        self.fractional
            .get_fractional_scale(&surface, qh, output.clone());

        self.river_status
            .get_river_output_status(&output, qh, output.clone());

        let display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(conn.backend().display_ptr().cast()).expect("shitface"),
        ));
        let surface_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(surface.id().as_ptr().cast()).expect("shitface"),
        ));

        let layer = self.layers.create_layer_surface(
            qh,
            surface,
            smithay_client_toolkit::shell::wlr_layer::Layer::Top,
            Some("desktop-things"),
            Some(&output),
        );

        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT);
        layer.set_exclusive_zone(WIDTH as i32);
        layer.set_size(WIDTH, 0);
        layer.commit();

        let surface = unsafe {
            self.instance
                .create_surface_unsafe(egui_wgpu::wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: display_handle,
                    raw_window_handle:  surface_handle,
                })
                .expect("Failed to create surface")
        };

        let root_taskbar_viewport = ViewportId::from_hash_of(&output);
        self.surfaces
            .insert(layer.wl_surface().clone(), root_taskbar_viewport);
        self.viewports
            .lock()
            .insert(root_taskbar_viewport, Viewport {
                parent: output.clone(),
                layer,
                surface,
                size: (WIDTH, 0),
            });

        let river_status = Arc::new(Mutex::new(RiverOutputStatus {
            used:       0,
            focused:    0,
            urgent:     0,
            view_title: String::new(),
        }));

        self.river_outputs.insert(output, river_status.clone());

        // TODO: Move this ui func somewhere else
        self.context.show_viewport_deferred(
            root_taskbar_viewport,
            ViewportBuilder::default()
                .with_transparent(true)
                .with_decorations(false),
            move |ctx, _| {
                let frame = egui::Frame::new()
                    .inner_margin(Margin::symmetric(2, 4))
                    .outer_margin(Margin::symmetric(2, 2))
                    .corner_radius(CornerRadius::same(10))
                    .fill(egui::Color32::from_gray(50));

                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .inner_margin(Margin::symmetric(2, 0))
                            .corner_radius(CornerRadius::same(10))
                            .fill(egui::Color32::from_gray(0xaa).gamma_multiply(0.3)),
                    )
                    .show(ctx, |ui| {
                        let river_status = river_status.lock();

                        let total_vertical = ui.available_height();

                        ui.vertical_centered(|ui| {
                            frame.show(ui, |ui| {
                                let mut job = LayoutJob {
                                    wrap: TextWrapping::from_wrap_mode_and_width(
                                        egui::TextWrapMode::Truncate,
                                        ui.available_height() / 3.0,
                                    ),
                                    ..Default::default()
                                };
                                RichText::new(river_status.view_title.as_str())
                                    .size(16.0)
                                    .strong()
                                    .color(Color32::WHITE)
                                    .append_to(
                                        &mut job,
                                        ui.style(),
                                        FontSelection::Default,
                                        egui::Align::Center,
                                    );

                                let galley = ui.painter().layout_job(job);

                                let rotation = Rot2::from_angle(PI / 2.0);

                                let (rect, _) = {
                                    let bounding_rect =
                                        Rect::from_center_size(Pos2::ZERO, galley.size())
                                            .rotate_bb(rotation);
                                    ui.allocate_exact_size(bounding_rect.size(), Sense::empty())
                                };

                                if ui.is_rect_visible(rect) {
                                    let pos = rect.center() - (rotation * (galley.size() / 2.0));

                                    ui.painter().add(TextShape {
                                        angle: PI / 2.0,
                                        ..TextShape::new(pos, galley, egui::Color32::PLACEHOLDER)
                                    });
                                }
                            });
                        });

                        ui.with_layout(
                            Layout::centered_and_justified(egui::Direction::TopDown)
                                .with_main_justify(false)
                                .with_main_align(egui::Align::Center),
                            |ui| {
                                for n in (0..31).filter(|i| river_status.used & (1 << i) != 0) {
                                    ui.add(
                                        egui::Button::new(format!("{n}"))
                                            .corner_radius(5)
                                            .fill(egui::Color32::from_gray(50)),
                                    );
                                }
                            },
                        );

                        ui.with_layout(Layout::bottom_up(egui::Align::Center), |ui| {
                            frame.show(ui, |ui| {
                                ui.button("henlo :3");
                            })
                        })
                    });
            },
        );
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
        output: smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput,
    ) {
        // Drop all surfaces associated with output
        self.viewports.lock().retain(|id, viewport| {
            if viewport.parent == output {
                let _ = self.input.lock().viewports.remove(id);
                self.surfaces.remove(viewport.layer.wl_surface());
                viewport.layer.wl_surface().destroy();

                false
            } else {
                true
            }
        });

        // Probably not needed?
        output.release();
    }
}

impl SeatHandler for Taskbar {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }

    fn new_seat(
        &mut self,
        _conn: &Connection,
        qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        seat: smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat,
    ) {
        info!(?seat, "New Seat");
        self.river_status
            .get_river_seat_status(&seat, qh, seat.clone());
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        seat: smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat,
        capability: smithay_client_toolkit::seat::Capability,
    ) {
        // Tell river to add seat if not already added
        self.river_status
            .get_river_seat_status(&seat, qh, seat.clone());

        //trace!(?seat, ?capability, "Updated seat capability");
        if matches!(capability, Capability::Pointer) {
            let _pointer = seat.get_pointer(qh, PointerData::new(seat.clone()));
        }
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

impl PointerHandler for Taskbar {
    #[allow(clippy::cast_possible_truncation)]
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &wayland_client::QueueHandle<Self>,
        _pointer: &wayland_client::protocol::wl_pointer::WlPointer,
        events: &[smithay_client_toolkit::seat::pointer::PointerEvent],
    ) {
        let mut interacted = Vec::new();
        self.input.lock().events.extend(events.iter().map(
            |PointerEvent {
                 surface,
                 position: (x, y),
                 kind,
             }| {
                interacted.push(surface.clone());
                match kind {
                    smithay_client_toolkit::seat::pointer::PointerEventKind::Leave {
                        serial: _,
                    } => egui::Event::PointerGone,
                    smithay_client_toolkit::seat::pointer::PointerEventKind::Motion { time: _ }
                    | smithay_client_toolkit::seat::pointer::PointerEventKind::Enter {
                        serial: _,
                    } => egui::Event::PointerMoved(Pos2::new(*x as f32, *y as f32)),
                    smithay_client_toolkit::seat::pointer::PointerEventKind::Press {
                        time: _,
                        button,
                        serial: _,
                    } => egui::Event::PointerButton {
                        pos:       Pos2::new(*x as f32, *y as f32),
                        button:    match *button {
                            BTN_LEFT => egui::PointerButton::Primary,
                            BTN_RIGHT => egui::PointerButton::Secondary,
                            BTN_MIDDLE => egui::PointerButton::Middle,
                            BTN_SIDE => egui::PointerButton::Extra1,
                            BTN_EXTRA => egui::PointerButton::Extra2,
                            _ => unimplemented!(),
                        },
                        pressed:   true,
                        modifiers: Modifiers::default(),
                    },
                    smithay_client_toolkit::seat::pointer::PointerEventKind::Release {
                        time: _,
                        button,
                        serial: _,
                    } => egui::Event::PointerButton {
                        pos:       Pos2::new(*x as f32, *y as f32),
                        button:    match *button {
                            BTN_LEFT => egui::PointerButton::Primary,
                            BTN_RIGHT => egui::PointerButton::Secondary,
                            BTN_MIDDLE => egui::PointerButton::Middle,
                            BTN_SIDE => egui::PointerButton::Extra1,
                            BTN_EXTRA => egui::PointerButton::Extra2,
                            _ => unimplemented!(),
                        },
                        pressed:   false,
                        modifiers: Modifiers::default(),
                    },
                    smithay_client_toolkit::seat::pointer::PointerEventKind::Axis {
                        time: _,
                        horizontal,
                        vertical,
                        source: _,
                    } => egui::Event::MouseWheel {
                        unit:      egui::MouseWheelUnit::Point,
                        delta:     Vec2::new(horizontal.absolute as f32, vertical.absolute as f32),
                        modifiers: Modifiers::NONE,
                    },
                }
            },
        ));

        interacted.dedup();
        for id in interacted
            .iter()
            .filter_map(|surface| self.surfaces.get(surface))
        {
            self.context.request_repaint_of(*id);
        }
    }
}

delegate_registry!(Taskbar);
delegate_seat!(Taskbar);
delegate_output!(Taskbar);
delegate_compositor!(Taskbar);
delegate_layer!(Taskbar);
delegate_pointer!(Taskbar);

impl ProvidesRegistryState for Taskbar {
    registry_handlers![OutputState, SeatState,];

    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
}
