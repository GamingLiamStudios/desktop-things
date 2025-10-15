use std::{
    collections::{
        BTreeMap,
        HashMap,
    },
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
                wl_output::WlOutput,
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
        SeatHandler,
        SeatState,
        keyboard::KeyboardHandler,
        pointer::PointerHandler,
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
use tracing::debug;
use wayland_backend::client::ObjectId;

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

        compositor,
        layer_shell,

        instance,
        render_state,

        outputs: BTreeMap::new(),
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
                    if let Some(layer) = ctx.outputs.get(&viewport_id) {
                        layer
                            .wl_surface()
                            .frame(&queue_handle, layer.wl_surface().clone());
                        layer.wl_surface().commit();
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

    compositor:  CompositorState,
    layer_shell: LayerShell,

    instance:     wgpu::Instance,
    render_state: RenderState,

    outputs:  BTreeMap<ViewportId, LayerSurface>,
    surfaces: HashMap<ObjectId, wgpu::Surface<'static>>,

    egui_context: egui::Context,
    egui_input:   egui::RawInput,
    render_fn:    Arc<dyn Fn(&egui::Context) + Send + Sync + 'static>,
}

impl Window {
    fn draw(
        &mut self,
        surface: &WlSurface,
        qh: &QueueHandle<Self>,
    ) {
        debug!(id = ?surface.id(), "Frame!");
        let mut input = self.egui_input.take();

        let viewport = ViewportId::from_hash_of(surface.id());
        input.viewport_id = viewport;
        let Some(callback) = self
            .egui_context
            .viewport_for(input.viewport_id, |viewport| {
                viewport.viewport_ui_cb.clone()
            })
        else {
            return;
        };
        let output = self.egui_context.run(input, callback.as_ref());

        let prims = self
            .egui_context
            .tessellate(output.shapes, output.pixels_per_point);

        let wgpu_surface = self
            .surfaces
            .get(&surface.id())
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
            pixels_per_point: output.pixels_per_point,
        };

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

            // FIXME: Get scaling factor properly
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
            self.outputs.remove(&viewport);
        }
    }
}

impl CompositorHandler for Window {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _surface: &smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface,
        _new_factor: i32,
    ) {
        // TODO
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _surface: &smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface,
        _new_transform: smithay_client_toolkit::reexports::client::protocol::wl_output::Transform,
    ) {
        // TODO
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: &WlSurface,
        _time: u32,
    ) {
        self.draw(surface, qh);
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _surface: &smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface,
        _output: &smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _surface: &smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface,
        _output: &smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput,
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
        self.outputs
            .insert(ViewportId::from_hash_of(layer.wl_surface().id()), layer);
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
        _ = self.outputs.remove(&ViewportId::from_hash_of(output.id()));
    }
}
impl SeatHandler for Window {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _seat: smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat,
    ) {
        //todo!()
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _seat: smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat,
        _capability: smithay_client_toolkit::seat::Capability,
    ) {
        //todo!()
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _seat: smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat,
        _capability: smithay_client_toolkit::seat::Capability,
    ) {
        //todo!()
    }

    fn remove_seat(
        &mut self,
        _conn: &Connection,
        _qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        _seat: smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat,
    ) {
        //todo!()
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
impl PointerHandler for Window {
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        qh: &smithay_client_toolkit::reexports::client::QueueHandle<Self>,
        pointer: &smithay_client_toolkit::reexports::client::protocol::wl_pointer::WlPointer,
        events: &[smithay_client_toolkit::seat::pointer::PointerEvent],
    ) {
        todo!()
    }
}
*/

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
                unsafe {
                    self.instance
                        .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                            raw_display_handle: display_handle,
                            raw_window_handle:  surface_handle,
                        })
                }
                .expect("Failed to create Wgpu Surface")
            });

        let (width, height) = configure.new_size;

        let cap = surface.get_capabilities(&self.render_state.adapter);
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

        surface.configure(&self.render_state.device, &surface_config);

        // Request frame redraw
        //layer.wl_surface().damage(0, 0, width as i32, height as i32);
        layer.wl_surface().frame(qh, layer.wl_surface().clone());
        layer.wl_surface().commit();

        let render_fn = self.render_fn.clone();
        let viewport = ViewportId::from_hash_of(layer.wl_surface().id());
        self.egui_context.show_viewport_deferred(
            viewport,
            ViewportBuilder::default().with_transparent(true),
            move |ctx, _class| (render_fn)(ctx),
        );
        self.egui_context.request_repaint_of(viewport);

        _ = self
            .egui_input
            .viewports
            .entry(viewport)
            .or_insert_with(|| ViewportInfo {
                parent: None,
                ..Default::default()
            });

        if first_draw {
            self.draw(layer.wl_surface(), qh);
        }

        debug!(id = ?layer.wl_surface().id(), width, height, "Configured");
    }
}

delegate_compositor!(Window);
delegate_output!(Window);

delegate_seat!(Window);
//delegate_keyboard!(Window);
//delegate_pointer!(Window);

delegate_layer!(Window);

delegate_registry!(Window);

impl ProvidesRegistryState for Window {
    registry_handlers![OutputState, SeatState,];

    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
}
