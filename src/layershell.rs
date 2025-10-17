use std::{
    collections::{
        BTreeMap,
        HashMap,
    },
    hash::Hash,
    ops::Deref,
    ptr::NonNull,
    sync::{
        Arc,
        RwLock,
    },
    time::Duration,
};

use egui::{
    RequestRepaintInfo,
    ViewportBuilder,
    ViewportCommand,
    ViewportId,
    ViewportInfo,
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
    activation::{
        ActivationHandler,
        ActivationState,
        RequestData,
    },
    compositor::{
        CompositorHandler,
        CompositorState,
    },
    delegate_compositor,
    delegate_keyboard,
    delegate_layer,
    delegate_output,
    delegate_pointer,
    delegate_registry,
    delegate_seat,
    delegate_shm,
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
            DispatchError,
            EventQueue,
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
        keyboard::KeyboardHandler,
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
    },
    shm::{
        Shm,
        ShmHandler,
    },
};
use tracing::{
    debug,
    warn,
};
use wayland_backend::client::ObjectId;
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

#[allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]
pub fn run<F>(render_fn: F) -> Result<(), Error>
where
    F: Fn(&egui::Context) + Sync + Send + 'static,
{
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

    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let queue_handle = event_queue.handle();

    //let (proxy, worker) = Proxy::new(conn.clone(), event_queue);

    let mut event_loop: EventLoop<Window> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    // SAFETY: We assert that LoopHandle is only used on the main thread and not
    // shared unsafely.

    // FIXME: Use mutex or something slightly safer than OH LORD HE COMIN'
    #[allow(clippy::arc_with_non_send_sync)]
    let unsafe_loop_handle = UnsafeLoopHandle(Arc::new(loop_handle.clone()));

    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle)
        .expect("Failed to insert EventLoop into WaylandSource");

    let compositor = CompositorState::bind(&globals, &queue_handle)?;
    let layer_shell = LayerShell::bind(&globals, &queue_handle)?;

    let wgpu_config = WgpuConfiguration {
        wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(WgpuSetupCreateNew {
            power_preference: egui_wgpu::wgpu::PowerPreference::LowPower,
            ..Default::default()
        }),
        ..Default::default()
    };

    let instance = wgpu::Instance::new(&InstanceDescriptor::from_env_or_default());

    let dummy_surface = compositor.create_surface(&queue_handle);
    let dummy_surface_wgpu = {
        let display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(conn.backend().display_ptr().cast()).expect("Wayland provided nullptr"),
        ));
        let surface_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(dummy_surface.id().as_ptr().cast()).expect("Wayland provided nullptr"),
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
        Some(&dummy_surface_wgpu),
        RendererOptions::default(),
    ))?;
    dummy_surface.destroy();

    let mut window = Window {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &queue_handle),
        output_state: OutputState::new(&globals, &queue_handle),
        fractional_manager: globals.bind(&queue_handle, 1..=1, GlobalData)?,

        pointers: HashMap::new(),

        compositor,
        layer_shell,

        instance,
        render_state,

        output_roots: HashMap::new(),
        surfaces: HashMap::new(),

        render_fn: Arc::new(render_fn),
        egui_context: egui::Context::default(),
        egui_input: egui::RawInput::default(),
    };

    window.egui_context.set_embed_viewports(false);
    window.egui_context.set_request_repaint_callback(
        move |RequestRepaintInfo {
                  viewport_id,
                  delay,
                  current_cumulative_pass_nr: _,
              }| {
            let timer = Timer::from_duration(delay);
            let queue_handle = queue_handle.clone();

            unsafe_loop_handle
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

    loop {
        event_loop.dispatch(None, &mut window)?;
        //debug!("new dispatch?");
    }
}

struct Window {
    registry_state: RegistryState,
    seat_state:     SeatState,
    output_state:   OutputState,

    fractional_manager: WpFractionalScaleManagerV1,

    pointers: HashMap<WlSeat, WlPointer>,

    compositor:  CompositorState,
    layer_shell: LayerShell,

    instance:     wgpu::Instance,
    render_state: RenderState,

    surfaces:     HashMap<ObjectId, Surface>,
    output_roots: HashMap<ObjectId, LayerSurface>,

    egui_context: egui::Context,
    egui_input:   egui::RawInput,
    render_fn:    Arc<dyn Fn(&egui::Context) + Send + Sync + 'static>,
}

struct Surface {
    viewport_id: ViewportId,

    wayland: WlSurface,
    wgpu:    wgpu::Surface<'static>,

    _fractional: WpFractionalScaleV1,
}

impl Window {
    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    fn draw(
        &mut self,
        surface: &WlSurface,
        _qh: &QueueHandle<Self>,
    ) {
        let mut input = self.egui_input.take();

        let Some(viewport) = self.surfaces.get(&surface.id()).map(|s| s.viewport_id) else {
            warn!("Viewport doesn't exist for surface");
            return;
        };
        let Some(callback) = self
            .egui_context
            .viewport_for(viewport, |viewport| viewport.viewport_ui_cb.clone())
        else {
            warn!("Immediate Callback");
            return;
        };

        let wgpu_surface = &self
            .surfaces
            .get(&surface.id())
            .expect("Surface not configured before WlSurface::frame")
            .wgpu;
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
        input.viewport_id = viewport;
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

        if !output.viewport_output.contains_key(&viewport) {
            self.surfaces.remove(&surface.id());
        }
    }
}

impl CompositorHandler for Window {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &WlSurface,
        new_factor: i32,
    ) {
        let Some(viewport) = self
            .egui_input
            .viewports
            .get_mut(&ViewportId::from_hash_of(surface.id()))
        else {
            return;
        };

        #[allow(clippy::cast_precision_loss)]
        let scale = new_factor as f32;
        viewport.native_pixels_per_point = Some(scale);
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
        qh: &QueueHandle<Self>,
        surface: &WlSurface,
        time: u32,
    ) {
        let time = Duration::from_millis(u64::from(time)).as_secs_f64();
        self.egui_input.time = Some(time);
        self.draw(surface, qh);
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

impl OutputHandler for Window {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
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
        self.output_roots.insert(output.id(), layer);
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
        let Some(layer) = self.output_roots.remove(&output.id()) else {
            return;
        };

        self.egui_context.send_viewport_cmd_to(
            ViewportId::from_hash_of(layer.wl_surface().id()),
            ViewportCommand::Close,
        );
    }
}
impl SeatHandler for Window {
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

impl PointerHandler for Window {
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
            let pos = if let Some(surface) = self.surfaces.get(&surface.id()) {
                let viewport = surface.viewport_id;
                let info = self
                    .egui_input
                    .viewports
                    .get(&viewport)
                    .expect("Egui doesn't contain viewport");

                let pixel_scale = (info.native_pixels_per_point.unwrap_or(1.0)
                    * self.egui_context.zoom_factor())
                .recip();
                egui::pos2(*pos_x as f32 * pixel_scale, *pos_y as f32 * pixel_scale)
            } else {
                egui::pos2(*pos_x as f32, *pos_y as f32)
            };

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

            self.egui_input.events.push(event);

            if let Some(surface) = self.surfaces.get(&surface.id()) {
                let viewport = surface.viewport_id;
                self.egui_context.request_repaint_of(viewport);
            }
        }
    }
}

impl LayerShellHandler for Window {
    fn closed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
    ) {
        let viewport = ViewportId::from_hash_of(layer.wl_surface().id());
        self.egui_context
            .send_viewport_cmd_to(viewport, ViewportCommand::Close);
    }

    fn configure(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let mut first_draw = false;

        let surface = self
            .surfaces
            .entry(layer.wl_surface().id())
            .or_insert_with(|| {
                first_draw = true;

                let display_handle = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
                    NonNull::new(conn.backend().display_ptr().cast())
                        .expect("Wayland provided nullptr"),
                ));
                let surface_handle = RawWindowHandle::Wayland(WaylandWindowHandle::new(
                    NonNull::new(layer.wl_surface().id().as_ptr().cast())
                        .expect("Wayland provided nullptr"),
                ));
                let surface = unsafe {
                    self.instance
                        .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                            raw_display_handle: display_handle,
                            raw_window_handle:  surface_handle,
                        })
                }
                .expect("Failed to create Wgpu Surface");

                let fractional = self.fractional_manager.get_fractional_scale(
                    layer.wl_surface(),
                    qh,
                    layer.wl_surface().clone(),
                );

                Surface {
                    viewport_id: ViewportId::from_hash_of(layer.wl_surface().id()),
                    wayland:     layer.wl_surface().clone(),
                    wgpu:        surface,

                    _fractional: fractional,
                }
            });

        let (width, height) = configure.new_size;

        let cap = surface.wgpu.get_capabilities(&self.render_state.adapter);
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

        surface
            .wgpu
            .configure(&self.render_state.device, &surface_config);

        // Request frame redraw
        //layer.wl_surface().damage(0, 0, width as i32, height as i32);
        layer.wl_surface().frame(qh, layer.wl_surface().clone());
        layer.wl_surface().commit();

        let render_fn = self.render_fn.clone();
        self.egui_context.show_viewport_deferred(
            surface.viewport_id,
            ViewportBuilder::default()
                .with_transparent(true)
                .with_decorations(false),
            move |ctx, _class| (render_fn)(ctx),
        );
        self.egui_context.request_repaint_of(surface.viewport_id);

        _ = self
            .egui_input
            .viewports
            .entry(surface.viewport_id)
            .or_insert_with(|| ViewportInfo {
                parent: Some(ViewportId::ROOT),
                ..Default::default()
            });

        if first_draw {
            self.draw(layer.wl_surface(), qh);
        }

        debug!(id = ?layer.wl_surface().id(), width, height, "Configured");
    }
}

impl Dispatch<WpFractionalScaleManagerV1, GlobalData> for Window {
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

impl Dispatch<WpFractionalScaleV1, WlSurface> for Window {
    fn event(
        state: &mut Self,
        _proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        data: &WlSurface,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wp_fractional_scale_v1::Event::PreferredScale { scale } => {
                let Some(viewport) = state
                    .egui_input
                    .viewports
                    .get_mut(&ViewportId::from_hash_of(data.id()))
                else {
                    return;
                };

                #[allow(clippy::cast_precision_loss)]
                let scale = scale as f32 / 120.0;
                viewport.native_pixels_per_point = Some(scale);
            },
            _ => unreachable!(),
        }
    }
}

delegate_compositor!(Window);
delegate_output!(Window);

delegate_seat!(Window);
//delegate_keyboard!(Window);
delegate_pointer!(Window);

delegate_layer!(Window);

delegate_registry!(Window);

impl ProvidesRegistryState for Window {
    registry_handlers![OutputState, SeatState,];

    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
}
