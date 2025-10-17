use std::{
    collections::HashMap,
    ops::Deref,
    ptr::NonNull,
    sync::Arc,
    time::Duration,
};

use egui::{
    RequestRepaintInfo,
    ViewportId,
};
use egui_wgpu::{
    RenderState,
    RendererOptions,
    ScreenDescriptor,
    WgpuConfiguration,
    WgpuError,
    WgpuSetupCreateNew,
    wgpu::{
        self,
        Color,
        InstanceDescriptor,
        Operations,
        RenderPassColorAttachment,
        rwh::{
            RawDisplayHandle,
            RawWindowHandle,
            WaylandDisplayHandle,
            WaylandWindowHandle,
        },
        wgt::CommandEncoderDescriptor,
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
        calloop::{
            self,
            EventLoop,
            LoopHandle,
            timer::{
                TimeoutAction,
                Timer,
            },
        },
        calloop_wayland_source::WaylandSource,
        client::{
            ConnectError,
            Connection,
            Dispatch,
            Proxy,
            QueueHandle,
            globals::{
                BindError,
                GlobalError,
                registry_queue_init,
            },
            protocol::{
                wl_output::{
                    Transform,
                    WlOutput,
                },
                wl_pointer::WlPointer,
                wl_seat::WlSeat,
                wl_surface::WlSurface,
            },
        },
    },
    registry::{
        ProvidesRegistryState,
        RegistryState,
    },
    registry_handlers,
    seat::{
        Capability,
        SeatHandler,
        SeatState,
        pointer::{
            PointerEvent,
            PointerEventKind,
            PointerHandler,
        },
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor,
            KeyboardInteractivity,
            Layer,
            LayerShell,
            LayerShellHandler,
            LayerSurface,
            LayerSurfaceConfigure,
        },
        xdg::{
            XdgShell,
            popup::{
                Popup,
                PopupConfigure,
                PopupHandler,
            },
            window::{
                Window as XdgWindow,
                WindowConfigure as XdgWindowConfigure,
                WindowHandler as XdgWindowHandler,
            },
        },
    },
};
use tracing::{
    debug,
    warn,
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::{
        self,
        WpFractionalScaleManagerV1,
    },
    wp_fractional_scale_v1::{
        self,
        WpFractionalScaleV1,
    },
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Wayland Connection Error")]
    WaylandConnect(#[from] ConnectError),
    #[error("Wayland Registry Error")]
    WaylandRegistry(#[from] GlobalError),

    #[error("Wayland Bind Error")]
    WaylandBind(#[from] BindError),

    #[error("Calloop Error")]
    Calloop(#[from] calloop::Error),

    #[error("Wgpu Error")]
    Wgpu(#[from] WgpuError),
}

#[derive(Clone)]
struct UnsafeLoopHandle<LH>(Arc<LH>);

#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl<LH> Send for UnsafeLoopHandle<LH> {}
unsafe impl<LH> Sync for UnsafeLoopHandle<LH> {}
impl<LH> Deref for UnsafeLoopHandle<LH> {
    type Target = LH;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

#[allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]
pub fn run<F>(render_fn: F) -> Result<(), Error>
where
    F: Fn(&egui::Context) + Sync + Send + 'static,
{
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let queue_handle = event_queue.handle();

    //let (proxy, worker) = Proxy::new(conn.clone(), event_queue);

    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    WaylandSource::new(conn, event_queue)
        .insert(loop_handle.clone())
        .expect("Failed to insert EventLoop into WaylandSource");

    #[allow(clippy::arc_with_non_send_sync)]
    let loop_handle = UnsafeLoopHandle(Arc::new(loop_handle.clone()));

    let compositor = CompositorState::bind(&globals, &queue_handle)?;
    let layer_shell = LayerShell::bind(&globals, &queue_handle)?;
    let xdg_shell = XdgShell::bind(&globals, &queue_handle)?;

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &queue_handle),
        output_state: OutputState::new(&globals, &queue_handle),
        fractional_manager: globals.bind(&queue_handle, 1..=1, GlobalData)?,

        pointers: HashMap::new(),
        loop_handle,

        compositor,
        layer_shell,
        xdg_shell,

        displays: HashMap::new(),
        surfaces: HashMap::new(),
        render_fn: Arc::new(render_fn),
    };

    loop {
        event_loop.dispatch(None, &mut app)?;
        //debug!("new dispatch?");
    }
}

struct App {
    loop_handle: UnsafeLoopHandle<LoopHandle<'static, Self>>,

    registry_state: RegistryState,
    seat_state:     SeatState,
    output_state:   OutputState,

    fractional_manager: WpFractionalScaleManagerV1,
    pointers:           HashMap<WlSeat, WlPointer>,

    compositor:  CompositorState,
    layer_shell: LayerShell,
    xdg_shell:   XdgShell,

    displays: HashMap<WlOutput, Display>,  // WlOutput
    surfaces: HashMap<WlSurface, Surface>, // WlSurface

    render_fn: Arc<dyn Fn(&egui::Context) + Send + Sync + 'static>,
}

struct Display {
    _instance:    wgpu::Instance,
    render_state: RenderState,
    layer_root:   LayerSurface,

    egui_context: egui::Context,
    egui_input:   egui::RawInput,

    scale:     f32,
    surfaces:  HashMap<WlSurface, wgpu::Surface<'static>>, // WlSurface
    render_fn: Arc<dyn Fn(&egui::Context) + Send + Sync + 'static>,
}

impl App {
    pub fn new_display(
        &self,
        conn: &Connection,
        layer: LayerSurface,
        qh: &QueueHandle<Self>,
        render_fn: Arc<dyn Fn(&egui::Context) + Send + Sync + 'static>,
    ) -> Display {
        let wgpu_config = WgpuConfiguration {
            wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(WgpuSetupCreateNew {
                power_preference: egui_wgpu::wgpu::PowerPreference::LowPower,
                ..Default::default()
            }),
            ..Default::default()
        };

        let instance = wgpu::Instance::new(&InstanceDescriptor::from_env_or_default());

        let surface_wgpu = {
            let display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
                NonNull::new(conn.backend().display_ptr().cast())
                    .expect("Wayland provided nullptr"),
            ));
            let surface_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
                NonNull::new(layer.wl_surface().id().as_ptr().cast())
                    .expect("Wayland provided nullptr"),
            ));
            unsafe {
                instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                    raw_display_handle: display_handle,
                    raw_window_handle:  surface_handle,
                })
            }
            .expect("Failed to create Wgpu Surface")
        };

        let render_state = pollster::block_on(RenderState::create(
            &wgpu_config,
            &instance,
            Some(&surface_wgpu),
            RendererOptions::default(),
        ))
        .expect("Failed to init wgpu instance");

        let ctx = egui::Context::default();

        let loop_handle = self.loop_handle.clone();
        let queue_handle = qh.clone();

        ctx.set_embed_viewports(false);
        ctx.set_request_repaint_callback(
            move |RequestRepaintInfo {
                      viewport_id,
                      delay,
                      current_cumulative_pass_nr: _,
                  }| {
                let timer = Timer::from_duration(delay);
                let queue_handle = queue_handle.clone();

                loop_handle
                    .insert_source(timer, move |_event, (), ctx| {
                        if let Some(layer) =
                            ctx.surfaces.values().find(|v| v.viewport_id == viewport_id)
                        {
                            layer.wayland.frame(&queue_handle, layer.wayland.clone());
                            layer.wayland.commit();
                        }

                        TimeoutAction::Drop
                    })
                    .expect("Failed to add request_repaint into eventloop");
            },
        );

        #[allow(clippy::mutable_key_type)]
        let mut surfaces = HashMap::new();
        surfaces.insert(layer.wl_surface().clone(), surface_wgpu);

        Display {
            _instance: instance,
            render_state,
            layer_root: layer,

            scale: 1.0,
            surfaces,

            egui_context: ctx,
            egui_input: egui::RawInput::default(),
            render_fn,
        }
    }
}

struct Surface {
    viewport_id: ViewportId,

    wayland: WlSurface,
    output:  WlOutput,

    _fractional: WpFractionalScaleV1,
}

impl Display {
    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    fn draw(
        &mut self,
        viewport_id: ViewportId,
        surface: &WlSurface,
    ) {
        let mut input = self.egui_input.take();

        let callback = if let Some(callback) = self
            .egui_context
            .viewport_for(viewport_id, |state| state.viewport_ui_cb.clone())
        {
            callback
        } else if viewport_id == ViewportId::ROOT {
            self.render_fn.clone()
        } else {
            warn!("Immediate Callback");
            return;
        };

        let wgpu_surface = &self
            .surfaces
            .get(surface)
            .expect("Surface not configured before WlSurface::frame");
        let surface_texture = wgpu_surface
            .get_current_texture()
            .expect("failed to acquire next swapchain texture");
        let texture_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let screen_descriptor = ScreenDescriptor {
            size_in_pixels:   [
                surface_texture.texture.width(),
                surface_texture.texture.height(),
            ],
            pixels_per_point: input.viewport().native_pixels_per_point.unwrap_or(1.0)
                * self.egui_context.zoom_factor(),
        };

        input.screen_rect = (screen_descriptor.size_in_pixels[0] > 0
            && screen_descriptor.size_in_pixels[1] > 0)
            .then(|| {
                egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::Vec2::new(
                        screen_descriptor.size_in_pixels[0] as f32
                            / screen_descriptor.pixels_per_point,
                        screen_descriptor.size_in_pixels[1] as f32
                            / screen_descriptor.pixels_per_point,
                    ),
                )
            });

        //debug!(events = ?input.events, ?viewport, time = ?input.time);
        input.viewport_id = viewport_id;
        let output = self.egui_context.run(input, callback.as_ref());

        let prims = self
            .egui_context
            .tessellate(output.shapes, screen_descriptor.pixels_per_point);

        let mut encoder =
            self.render_state
                .device
                .create_command_encoder(&CommandEncoderDescriptor {
                    label: Some("egui_encoder"),
                });

        {
            let mut state = self.render_state.renderer.write();

            for (id, delta) in output.textures_delta.set {
                state.update_texture(
                    &self.render_state.device,
                    &self.render_state.queue,
                    id,
                    &delta,
                );
            }

            state.update_buffers(
                &self.render_state.device,
                &self.render_state.queue,
                &mut encoder,
                &prims,
                &screen_descriptor,
            );
        }

        {
            let mut render_pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label:                    Some("egui_render"),
                    color_attachments:        &[Some(RenderPassColorAttachment {
                        view:           &texture_view,
                        depth_slice:    None,
                        resolve_target: None,
                        ops:            Operations {
                            load:  wgpu::LoadOp::Clear(Color::TRANSPARENT),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes:         None,
                    occlusion_query_set:      None,
                })
                .forget_lifetime();

            let state = self.render_state.renderer.read();
            state.render(&mut render_pass, &prims, &screen_descriptor);
        }

        self.render_state.queue.submit([encoder.finish()]);
        surface_texture.present();

        {
            let mut state = self.render_state.renderer.write();

            for id in output.textures_delta.free {
                state.free_texture(&id);
            }
        }

        // Request next frame
        // surface.frame(qh, surface.clone());
        // surface.commit();

        let mut to_remove = Vec::new();
        for surface in self.surfaces.keys() {
            if !output
                .viewport_output
                .contains_key(&ViewportId::from_hash_of(surface.id()))
                && surface != self.layer_root.wl_surface()
            {
                to_remove.push(surface.clone());
            }
        }

        for surface in to_remove {
            self.surfaces.remove(&surface);
            self.egui_input
                .viewports
                .remove(&ViewportId::from_hash_of(surface.id()));
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &WlSurface,
        new_factor: i32,
    ) {
        let Some(surface) = self.surfaces.get(surface) else {
            warn!("Unassigned WlSurface");
            return;
        };

        let Some(display) = self.displays.get_mut(&surface.output) else {
            warn!("Unassigned WlSurface");
            return;
        };

        #[allow(clippy::cast_precision_loss)]
        let scale = new_factor as f32;
        display.scale = scale;
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_transform: Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &WlSurface,
        time: u32,
    ) {
        let time = Duration::from_millis(u64::from(time)).as_secs_f64();

        let Some(surface) = self.surfaces.get(surface) else {
            warn!("Unassigned WlSurface");
            return;
        };

        let output = surface.output.clone();
        let Some(display) = self.displays.get_mut(&output) else {
            warn!("Unassigned WlSurface");
            return;
        };

        let Some(viewport_id) = self.surfaces.get(&surface.wayland).map(|s| s.viewport_id) else {
            warn!("Viewport doesn't exist for surface");
            return;
        };

        display.egui_input.time = Some(time);
        display.draw(viewport_id, &surface.wayland);

        self.surfaces.retain(|_, surface| {
            surface.output != output
                || display
                    .egui_input
                    .viewports
                    .contains_key(&ViewportId::from_hash_of(surface.wayland.id()))
                || *display.layer_root.wl_surface() == surface.wayland
        });
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        output: WlOutput,
    ) {
        // TODO: Layer Options
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Top,
            Some("desktop_things"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT);
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
        layer.set_size(40, 0);
        layer.set_exclusive_zone(40);
        layer.commit();

        debug!(id = ?output.id(), surface = ?layer.wl_surface().id(), "New Output");
        let fractional = self.fractional_manager.get_fractional_scale(
            layer.wl_surface(),
            qh,
            layer.wl_surface().clone(),
        );

        self.surfaces.insert(layer.wl_surface().clone(), Surface {
            viewport_id: ViewportId::ROOT,
            wayland:     layer.wl_surface().clone(),
            output:      output.clone(),

            _fractional: fractional,
        });

        self.displays.insert(
            output,
            self.new_display(conn, layer, qh, self.render_fn.clone()),
        );
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: WlOutput,
    ) {
        debug!(id = ?output.id(), "Update Output");
        // Pretty sure we don't need to worry bout this?
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: WlOutput,
    ) {
        _ = self.displays.remove(&output);
    }
}
impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: WlSeat,
    ) {
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: smithay_client_toolkit::seat::Capability,
    ) {
        if capability == Capability::Pointer {
            let pointer = self
                .seat_state
                .get_pointer(qh, &seat)
                .expect("Failed to get WlSeat::Pointer");

            self.pointers.insert(seat, pointer);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: smithay_client_toolkit::seat::Capability,
    ) {
        if capability == Capability::Pointer {
            self.pointers.remove(&seat);
        }
    }

    fn remove_seat(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        seat: WlSeat,
    ) {
        self.pointers.remove(&seat);
    }
}

/*
impl KeyboardHandler for Window {
    fn enter(
        &mut self,
        conn: &Connection,
        qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        keyboard: &smithay_client_toolkit::reexports::client::protocol::wl_keyboard::WlKeyboard,
        surface: &smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface,
        serial: u32,
        raw: &[u32],
        keysyms: &[smithay_client_toolkit::seat::keyboard::Keysym],
    ) {
        todo!()
    }

    fn leave(
        &mut self,
        conn: &Connection,
        qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        keyboard: &smithay_client_toolkit::reexports::client::protocol::wl_keyboard::WlKeyboard,
        surface: &smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface,
        serial: u32,
    ) {
        todo!()
    }

    fn press_key(
        &mut self,
        conn: &Connection,
        qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        keyboard: &smithay_client_toolkit::reexports::client::protocol::wl_keyboard::WlKeyboard,
        serial: u32,
        event: smithay_client_toolkit::seat::keyboard::KeyEvent,
    ) {
        todo!()
    }

    fn repeat_key(
        &mut self,
        conn: &Connection,
        qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        keyboard: &smithay_client_toolkit::reexports::client::protocol::wl_keyboard::WlKeyboard,
        serial: u32,
        event: smithay_client_toolkit::seat::keyboard::KeyEvent,
    ) {
        todo!()
    }

    fn release_key(
        &mut self,
        conn: &Connection,
        qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        keyboard: &smithay_client_toolkit::reexports::client::protocol::wl_keyboard::WlKeyboard,
        serial: u32,
        event: smithay_client_toolkit::seat::keyboard::KeyEvent,
    ) {
        todo!()
    }

    fn update_modifiers(
        &mut self,
        conn: &Connection,
        qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        keyboard: &smithay_client_toolkit::reexports::client::protocol::wl_keyboard::WlKeyboard,
        serial: u32,
        modifiers: smithay_client_toolkit::seat::keyboard::Modifiers,
        raw_modifiers: smithay_client_toolkit::seat::keyboard::RawModifiers,
        layout: u32,
    ) {
        todo!()
    }
}
*/

const fn from_raw_button(code: u32) -> egui::PointerButton {
    use smithay_client_toolkit::seat::pointer::{
        BTN_BACK,
        BTN_FORWARD,
        BTN_MIDDLE,
        BTN_RIGHT,
    };

    match code {
        BTN_RIGHT => egui::PointerButton::Secondary,
        BTN_MIDDLE => egui::PointerButton::Middle,
        BTN_BACK => egui::PointerButton::Extra1,
        BTN_FORWARD => egui::PointerButton::Extra2,
        _ => egui::PointerButton::Primary,
    }
}

impl PointerHandler for App {
    #[allow(clippy::cast_possible_truncation)]
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        for PointerEvent {
            surface,
            position: (pos_x, pos_y),
            kind,
        } in events
        {
            let Some(surface) = self.surfaces.get(surface) else {
                warn!("Unregistered WlSurface");
                continue;
            };

            let Some(display) = self.displays.get_mut(&surface.output) else {
                warn!("WlSurface attached to Unregistered WlOutput");
                continue;
            };

            let pixel_scale = (display.scale * display.egui_context.zoom_factor()).recip();
            let pos = egui::pos2(*pos_x as f32 * pixel_scale, *pos_y as f32 * pixel_scale);

            let event = match kind {
                PointerEventKind::Enter { serial: _ } | PointerEventKind::Motion { time: _ } => {
                    egui::Event::PointerMoved(pos)
                },
                PointerEventKind::Axis {
                    time: _,
                    horizontal,
                    vertical,
                    source: _,
                } => egui::Event::MouseWheel {
                    unit:      egui::MouseWheelUnit::Point,
                    delta:     egui::Vec2 {
                        x: horizontal.absolute as f32,
                        y: vertical.absolute as f32,
                    },
                    modifiers: egui::Modifiers::default(), // FIXME: Modifiers
                },
                PointerEventKind::Press {
                    time: _,
                    button,
                    serial: _,
                } => egui::Event::PointerButton {
                    pos,
                    button: from_raw_button(*button),
                    pressed: true,
                    modifiers: egui::Modifiers::default(), // FIXME: Modifiers
                },
                PointerEventKind::Release {
                    time: _,
                    button,
                    serial: _,
                } => egui::Event::PointerButton {
                    pos,
                    button: from_raw_button(*button),
                    pressed: false,
                    modifiers: egui::Modifiers::default(), // FIXME: Modifiers
                },
                PointerEventKind::Leave { serial: _ } => egui::Event::PointerGone,
            };

            display.egui_input.events.push(event);

            if let Some(surface) = self.surfaces.get(&surface.wayland) {
                let viewport = surface.viewport_id;
                display.egui_context.request_repaint_of(viewport);
            }
        }
    }
}

impl LayerShellHandler for App {
    fn closed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
    ) {
        let output = {
            let Some(surface) = self.surfaces.get(layer.wl_surface()) else {
                warn!("Unregistered WlSurface");
                return;
            };

            let output = surface.output.clone();
            if let Some(display) = self.displays.get(&output) {
                self.surfaces
                    .retain(|id, _| !display.surfaces.contains_key(id));
            }

            output
        };

        self.displays.remove(&output);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(surface) = self.surfaces.get(layer.wl_surface()) else {
            warn!("Unregistered WlSurface");
            return;
        };

        let Some(display) = self.displays.get_mut(&surface.output) else {
            warn!("WlSurface attached to Unregistered WlOutput");
            return;
        };

        let Some(surface_wgpu) = display.surfaces.get(&surface.wayland) else {
            warn!("WGPU surface not created!");
            return;
        };

        let (width, height) = configure.new_size;

        let cap = surface_wgpu.get_capabilities(&display.render_state.adapter);
        let prefrered_format = egui_wgpu::preferred_framebuffer_format(&cap.formats)
            .expect("No supported Framebuffer format");

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: prefrered_format,
            view_formats: vec![prefrered_format],
            alpha_mode: wgpu::CompositeAlphaMode::PreMultiplied,
            width,
            height,
            desired_maximum_frame_latency: 2,
            // Wayland is inherently a mailbox system.
            present_mode: wgpu::PresentMode::Mailbox,
        };
        surface_wgpu.configure(&display.render_state.device, &surface_config);

        display.draw(ViewportId::ROOT, &surface.wayland);
        display.egui_context.request_repaint_of(ViewportId::ROOT);

        debug!(id = ?layer.wl_surface().id(), width, height, "Configured");
    }
}

impl XdgWindowHandler for App {
    fn request_close(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &XdgWindow,
    ) {
        todo!()
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &XdgWindow,
        _configure: smithay_client_toolkit::shell::xdg::window::WindowConfigure,
        _serial: u32,
    ) {
        todo!()
    }
}

impl PopupHandler for App {
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _popup: &Popup,
        _config: PopupConfigure,
    ) {
        todo!()
    }

    fn done(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _popup: &Popup,
    ) {
        todo!()
    }
}

impl Dispatch<WpFractionalScaleManagerV1, GlobalData> for App {
    fn event(
        _state: &mut Self,
        _proxy: &WpFractionalScaleManagerV1,
        _event: wp_fractional_scale_manager_v1::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        unreachable!()
    }
}

impl Dispatch<WpFractionalScaleV1, WlSurface> for App {
    fn event(
        state: &mut Self,
        _proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        surface: &WlSurface,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wp_fractional_scale_v1::Event::PreferredScale { scale } => {
                let Some(surface) = state.surfaces.get(surface) else {
                    warn!("Unassigned WlSurface");
                    return;
                };

                let Some(display) = state.displays.get_mut(&surface.output) else {
                    warn!("Unassigned WlSurface");
                    return;
                };

                #[allow(clippy::cast_precision_loss)]
                let scale = scale as f32 / 120.0;
                display.scale = scale;
            },
            _ => unreachable!(),
        }
    }
}

delegate_compositor!(App);
delegate_output!(App);

delegate_seat!(App);
//delegate_keyboard!(Window);
delegate_pointer!(App);

delegate_layer!(App);
delegate_xdg_shell!(App);
delegate_xdg_popup!(App);

delegate_registry!(App);

impl ProvidesRegistryState for App {
    registry_handlers![OutputState, SeatState,];

    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
}
