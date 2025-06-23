#![allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)]
use core::f32;
use std::{
    self,
    collections::HashMap,
    f32::consts::PI,
    io::{
        Read,
        Seek,
    },
    ptr::NonNull,
    sync::Arc,
    time::{
        Duration,
        Instant,
    },
};

use egui::{
    emath::Rot2,
    epaint::{
        CircleShape,
        PathStroke,
        TextShape,
    },
    lerp,
    mutex::Mutex,
    text::{
        LayoutJob,
        TextWrapping,
    },
    Align2,
    Color32,
    CornerRadius,
    FontSelection,
    FullOutput,
    Layout,
    Margin,
    Mesh,
    Modifiers,
    Pos2,
    ProgressBar,
    RawInput,
    Rect,
    RequestRepaintInfo,
    RichText,
    Sense,
    Shape,
    Stroke,
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
        Color,
        CommandEncoderDescriptor,
        PresentMode,
        TextureFormat,
    },
    RenderState,
    ScreenDescriptor,
    SurfaceErrorAction,
};
use river_control_unstable_v1::{
    zriver_command_callback_v1::{
        self,
        ZriverCommandCallbackV1,
    },
    zriver_control_v1::{
        self,
        ZriverControlV1,
    },
};
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
    delegate_xdg_popup,
    delegate_xdg_shell,
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
        protocols::{
            wp::fractional_scale::v1::client::{
                wp_fractional_scale_manager_v1::{
                    self,
                    WpFractionalScaleManagerV1,
                },
                wp_fractional_scale_v1::{
                    self,
                    WpFractionalScaleV1,
                },
            },
            xdg::shell::client::{
                xdg_popup::{
                    self,
                    XdgPopup,
                },
                xdg_positioner::{
                    self,
                    XdgPositioner,
                },
                xdg_surface::{
                    self,
                    XdgSurface,
                },
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
        xdg::{
            popup::PopupHandler,
            window::WindowHandler,
            XdgShell,
        },
        WaylandSurface,
    },
};
use smol::Executor;
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
        //egui::Context::set_immediate_viewport_renderer(callback);

        let mut taskbar = Taskbar {
            registry: RegistryState::new(&globals),
            seat: SeatState::new(&globals, &qh),
            output: OutputState::new(&globals, &qh),
            xdg_shell: XdgShell::bind(&globals, &qh)?,

            compositor: CompositorState::bind(&globals, &qh)?,
            layers: LayerShell::bind(&globals, &qh)?,
            fractional: globals.bind(&qh, 1..=1, GlobalData)?,

            river_status: globals.bind(&qh, 4..=4, GlobalData)?,
            river_outputs: HashMap::new(),
            river_focus: Arc::new(Mutex::new(HashMap::new())),

            river_control: globals.bind(&qh, 1..=1, GlobalData)?,

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
                let qh = qh.clone();

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

                        viewport
                            .layer
                            .wl_surface()
                            .frame(&qh, viewport.layer.wl_surface().clone());
                        viewport.layer.wl_surface().commit();
                    }
                    drop(viewports);

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
    xdg_shell:  XdgShell,

    river_control: ZriverControlV1,
    river_status:  ZriverStatusManagerV1,
    river_outputs: HashMap<WlOutput, Arc<Mutex<RiverOutputStatus>>>,
    river_focus:   Arc<Mutex<HashMap<WlSeat, WlOutput>>>,

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
                    if tags.is_empty() {
                        status.used = 0;
                    } else {
                        // List of views & their tags
                        let views: &[u32] = bytemuck::cast_slice(&tags); // TODO: Do nicer things later with this info
                        status.used = views.iter().fold(0u32, |acc, v| acc | v);
                    }
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
                state.river_focus.lock().insert(seat.clone(), output);
            },
            zriver_seat_status_v1::Event::UnfocusedOutput { output: _ } => {
                // Assuming seat cannot focus more than one output (for convenience sake)
                state.river_focus.lock().remove(seat);
            },
            zriver_seat_status_v1::Event::FocusedView { title } => {
                let river_focus = state.river_focus.lock();
                let focused = river_focus
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

impl Dispatch<ZriverControlV1, GlobalData> for Taskbar {
    fn event(
        _state: &mut Self,
        _proxy: &ZriverControlV1,
        _event: zriver_control_v1::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
    ) {
        unimplemented!("There are no events for RiverControlV1")
    }
}

impl Dispatch<ZriverCommandCallbackV1, GlobalData> for Taskbar {
    fn event(
        _state: &mut Self,
        _proxy: &ZriverCommandCallbackV1,
        event: zriver_command_callback_v1::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
    ) {
        match event {
            zriver_command_callback_v1::Event::Failure { failure_message } => {
                warn!(failure_message);
            },
            zriver_command_callback_v1::Event::Success { output: _ } => {},
        }
    }
}

impl Dispatch<XdgSurface, GlobalData> for Taskbar {
    fn event(
        _state: &mut Self,
        _proxy: &XdgSurface,
        event: xdg_surface::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        match event {
            xdg_surface::Event::Configure { serial: _ } => {},
            _ => unreachable!(),
        }
    }
}

impl Dispatch<XdgPositioner, GlobalData> for Taskbar {
    fn event(
        _state: &mut Self,
        _proxy: &XdgPositioner,
        _event: xdg_positioner::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgPopup, GlobalData> for Taskbar {
    fn event(
        _state: &mut Self,
        _proxy: &XdgPopup,
        _event: xdg_popup::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &wayland_client::QueueHandle<Self>,
    ) {
        todo!()
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
        _surface: &smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface,
        time: u32,
    ) {
        self.input.lock().time = Some(Duration::from_millis(u64::from(time)).as_secs_f64());
        // if let Some(&id) = self.surfaces.get(surface) {
        //     self.context.request_repaint_of(id);
        // }
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

        let positioner = self
            .xdg_shell
            .xdg_wm_base()
            .create_positioner(qh, GlobalData);
        positioner.set_anchor(xdg_positioner::Anchor::TopLeft);

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

        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::RIGHT);
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

        self.river_outputs
            .insert(output.clone(), river_status.clone());

        let river_control = self.river_control.clone();
        let qh = qh.clone();
        let river_focus = self.river_focus.clone();

        //let compositor = self.compositor.clone();
        //let fractional = self.fractional.clone();
        //let xdg_shell = self.xdg_shell.xdg_wm_base().clone();

        // TODO: Move this ui func somewhere else
        self.context.show_viewport_deferred(
            root_taskbar_viewport,
            ViewportBuilder::default()
                .with_transparent(true)
                .with_decorations(false),
            move |ctx, _| {
                let frame = egui::Frame::new()
                    .inner_margin(Margin::symmetric(0, 4))
                    .outer_margin(Margin::symmetric(2, 2))
                    .corner_radius(CornerRadius::same(10))
                    .fill(egui::Color32::from_gray(50));

                let output = output.clone();
                let qh = qh.clone();
                let river_status = river_status.clone();
                let river_control = river_control.clone();
                let river_focus = river_focus.clone();

                let mut charge_full =
                    std::fs::File::open("/sys/class/power_supply/BAT1/charge_full")
                        .expect("Failed to open battery sysfs");
                let mut charge_now = std::fs::File::open("/sys/class/power_supply/BAT1/charge_now")
                    .expect("Failed to open battery sysfs");

                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .inner_margin(Margin::symmetric(2, 0))
                            .corner_radius(CornerRadius::same(10))
                            .fill(egui::Color32::from_gray(0xaa).gamma_multiply(0.3)),
                    )
                    .show(ctx, move |ui| {
                        ui.style_mut().visuals.panel_fill = egui::Color32::from_gray(50);
                        ui.style_mut().visuals.window_fill = egui::Color32::from_gray(50);
                        ui.style_mut().visuals.override_text_color = Some(egui::Color32::WHITE);

                        let full_size = ctx
                            .available_rect()
                            .with_min_x(2.0)
                            .with_max_x(ctx.available_rect().max.x - 2.0);
                        let (top, remain) = full_size.split_top_bottom_at_fraction(1.0 / 3.0);
                        let (middle, _bottom) = remain.split_top_bottom_at_fraction(0.5);

                        ui.add_sized(top.size(), {
                            let river_status = river_status.clone();
                            move |ui: &mut egui::Ui| {
                                let river_status = river_status.lock();
                                ui.vertical_centered(|ui| {
                                    frame.inner_margin(8).show(ui, move |ui| {
                                        let mut job = LayoutJob {
                                            wrap: TextWrapping::from_wrap_mode_and_width(
                                                egui::TextWrapMode::Truncate,
                                                ui.available_height(),
                                            ),
                                            ..Default::default()
                                        };
                                        RichText::new(river_status.view_title.as_str())
                                            .size(16.0)
                                            .strong()
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
                                            ui.allocate_exact_size(
                                                bounding_rect.size(),
                                                Sense::empty(),
                                            )
                                        };

                                        if ui.is_rect_visible(rect) {
                                            let pos =
                                                rect.center() - (rotation * (galley.size() / 2.0));

                                            ui.painter().add(TextShape {
                                                angle: PI / 2.0,
                                                ..TextShape::new(
                                                    pos,
                                                    galley,
                                                    egui::Color32::PLACEHOLDER,
                                                )
                                            });
                                        }
                                    });
                                    ui.add_space(ui.available_height());
                                })
                                .response
                            }
                        });

                        let river_status = river_status.clone();
                        let river_focus = river_focus.clone();
                        let river_control = river_control.clone();

                        ui.add_sized(middle.size(), move |ui: &mut egui::Ui| {
                            egui::Frame::NONE
                                .show(ui, move |ui| {
                                    let river_status = river_status.lock();
                                    let focused =
                                        (0..31).filter(|&i| river_status.used & (1 << i) != 0);

                                    ui.spacing_mut().item_spacing = Vec2::new(0.0, 1.0);

                                    ui.with_layout(
                                        Layout::top_down(egui::Align::Center)
                                            .with_main_align(egui::Align::Max),
                                        |ui| {
                                            let width = ui.available_width() - 0.0;

                                            // Add space before
                                            let button_space = width.mul_add(0.6, 1.0)
                                                * focused.clone().count() as f32;
                                            ui.add_space(
                                                (ui.available_height() - button_space) / 2.0,
                                            );

                                            for tag in focused {
                                                if egui::Frame::NONE
                                                    .outer_margin(Margin {
                                                        left: 2,
                                                        ..Margin::same(0)
                                                    })
                                                    .show(ui, |ui| {
                                                        ui.add(
                                                            egui::Button::new(
                                                                RichText::new(format!(
                                                                    "{}",
                                                                    tag + 1
                                                                ))
                                                                .size(16.0)
                                                                .strong(),
                                                            )
                                                            .min_size(Vec2::new(width, width * 0.6))
                                                            .corner_radius(5)
                                                            .fill(egui::Color32::from_gray(50)),
                                                        )
                                                    })
                                                    .inner
                                                    .clicked()
                                                {
                                                    // Attempt find seat attached to
                                                    // current
                                                    // viewport (output)
                                                    if let Some((seat, _)) = river_focus
                                                        .lock()
                                                        .iter()
                                                        .find(|&(_, v)| *v == output)
                                                    {
                                                        river_control.add_argument(
                                                            "set-focused-tags".to_string(),
                                                        );
                                                        river_control
                                                            .add_argument(format!("{}", 1 << tag));
                                                        river_control
                                                            .run_command(seat, &qh, GlobalData);
                                                    }
                                                }
                                            }
                                        },
                                    );
                                })
                                .response
                        });

                        //let output = output.clone();
                        //let qh = qh.clone();

                        //ui.add_space(bottom.size().y - sized.size().y);
                        ui.with_layout(Layout::bottom_up(egui::Align::Center), move |ui| {
                            ui.spacing_mut().item_spacing = Vec2::new(0.0, 2.0);

                            let local_time = chrono::Local::now();
                            frame
                                .inner_margin(Margin {
                                    left: 0,
                                    ..frame.inner_margin
                                })
                                .show(ui, |ui| {
                                    let clock = ui.label(
                                        RichText::new(local_time.format("%I\n%M").to_string())
                                            .size(16.0)
                                            .strong(),
                                    );
                                    if clock.contains_pointer() {
                                        /*
                                        let surface = compositor.create_surface(&qh);
                                        fractional.get_fractional_scale(
                                            &surface,
                                            &qh,
                                            output.clone(),
                                        );

                                        let surface_handle =
                                            RawWindowHandle::Wayland(WaylandWindowHandle::new(
                                                NonNull::new(surface.id().as_ptr().cast())
                                                    .expect("shitface"),
                                            ));

                                        let xdg_surface =
                                            xdg_shell.get_xdg_surface(&surface, &qh, GlobalData);
                                        let popup = xdg_surface.get_popup(
                                            None,
                                            &positioner,
                                            &qh,
                                            GlobalData,
                                        );


                                                            let popup_viewport = ViewportId::from_hash_of(&surface);
                                                            self.surfaces.insert(surface.clone(), popup_viewport);

                                                            let surface = unsafe {
                                                                self.instance
                                        .create_surface_unsafe(egui_wgpu::wgpu::SurfaceTargetUnsafe::RawHandle {
                                            raw_display_handle: display_handle,
                                            raw_window_handle:  surface_handle,
                                        })
                                        .expect("Failed to create surface")
                                                            };
                                                            self.viewports.lock().insert(popup_viewport, Viewport {
                                                                parent: output.clone(),
                                                                layer,
                                                                surface,
                                                                size: (WIDTH, 0),
                                                            });

                                                            self.context.show_viewport_deferred(
                                                                popup_viewport,
                                                                ViewportBuilder::default()
                                                                    .with_always_on_top()
                                                                    .with_window_level(egui::WindowLevel::AlwaysOnTop)
                                                                    .with_position(clock.rect.right_top())
                                                                    .with_inner_size([10.0, 20.0]),
                                                                |ctx, _| {
                                                                    egui::CentralPanel::default()
                                                                        .frame(egui::Frame::NONE)
                                                                        .show(ctx, |ui| {
                                                                            ui.label("i fucked your mom shitlips")
                                                                        });
                                                                },
                                                            );
                                                            */
                                    }
                                });

                            frame.show(ui, |ui| {
                                // Get current battery level
                                let max: usize = {
                                    let mut string = String::new();
                                    let _ = charge_full.read_to_string(&mut string);

                                    string.trim().parse().expect("shitface")
                                };

                                let now: usize = {
                                    let mut string = String::new();
                                    let _ = charge_now.read_to_string(&mut string);

                                    string.trim().parse().expect("shitface")
                                };

                                let percentage = now as f64 / max as f64;
                                let avail_width = ui.available_size_before_wrap().x;

                                let rect = ui
                                    .allocate_ui([avail_width; 2].into(), |ui| {
                                        ui.label("Test");
                                        ui.allocate_space(ui.available_size())
                                    })
                                    .response
                                    .rect;

                                let num_points = 100;
                                let start_angle = f32::consts::FRAC_PI_4;
                                let half_height = avail_width / 2.0;

                                let mut mesh = Mesh::default();
                                mesh.colored_vertex(rect.center(), Color32::TRANSPARENT);
                                (0..num_points).for_each(|i| {
                                    let angle = lerp(
                                        0.0..=2.0f32
                                            .mul_add(-f32::consts::FRAC_PI_4, f32::consts::TAU),
                                        i as f32 / num_points as f32,
                                    );
                                    let (sin, cos) = (angle - start_angle).sin_cos();
                                    let point = rect.center()
                                        + (half_height - 2.0) * Vec2 { x: cos, y: sin };

                                    let color = Color32::LIGHT_GREEN.lerp_to_gamma(
                                        Color32::WHITE,
                                        i as f32 / num_points as f32,
                                    );
                                    mesh.colored_vertex(point, color);
                                });

                                let num_used = (percentage * num_points as f64) as u32;
                                for i in 0..(num_used - 1) {
                                    mesh.add_triangle(i, 0, i + 1);
                                }
                                ui.painter().add(Shape::mesh(mesh));
                            });
                            ui.separator();
                            frame.show(ui, |ui| ui.label("Dock"));
                        });
                    });

                ctx.request_repaint_after(Duration::from_secs_f32(
                    30.0 - Instant::now().elapsed().as_secs_f32().fract(),
                ));
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

impl PopupHandler for Taskbar {
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &wayland_client::QueueHandle<Self>,
        _popup: &smithay_client_toolkit::shell::xdg::popup::Popup,
        _config: smithay_client_toolkit::shell::xdg::popup::PopupConfigure,
    ) {
        todo!()
    }

    fn done(
        &mut self,
        _conn: &Connection,
        _qh: &wayland_client::QueueHandle<Self>,
        _popup: &smithay_client_toolkit::shell::xdg::popup::Popup,
    ) {
        todo!()
    }
}

impl WindowHandler for Taskbar {
    fn request_close(
        &mut self,
        _conn: &Connection,
        _qh: &wayland_client::QueueHandle<Self>,
        _window: &smithay_client_toolkit::shell::xdg::window::Window,
    ) {
        unimplemented!()
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &wayland_client::QueueHandle<Self>,
        _window: &smithay_client_toolkit::shell::xdg::window::Window,
        _configure: smithay_client_toolkit::shell::xdg::window::WindowConfigure,
        _serial: u32,
    ) {
        unimplemented!()
    }
}

delegate_registry!(Taskbar);
delegate_seat!(Taskbar);
delegate_output!(Taskbar);
delegate_compositor!(Taskbar);
delegate_layer!(Taskbar);
delegate_pointer!(Taskbar);
delegate_xdg_shell!(Taskbar);
delegate_xdg_popup!(Taskbar);

impl ProvidesRegistryState for Taskbar {
    registry_handlers![OutputState, SeatState,];

    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
}
