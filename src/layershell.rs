use std::{
    collections::HashMap,
    ptr::NonNull,
    time::Duration,
};

use raw_window_handle::{
    RawDisplayHandle,
    RawWindowHandle,
    WaylandDisplayHandle,
    WaylandWindowHandle,
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
use vello::{
    Renderer,
    peniko::{
        Brush,
        color::palette,
    },
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

use crate::{
    InputEvent,
    Program,
    RenderContext,
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

    #[error("Request Adapter Error")]
    RequestAdapter(#[from] wgpu::RequestAdapterError),
    #[error("Request Device Error")]
    RequestDevice(#[from] wgpu::RequestDeviceError),

    #[error("Vello Renderer Error")]
    Vello(#[from] vello::Error),
}

#[allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]
pub fn run<P, F>(program_builder: F) -> Result<(), Error>
where
    P: Program + 'static,
    F: Fn() -> P + 'static,
{
    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let queue_handle = event_queue.handle();

    //let (proxy, worker) = Proxy::new(conn.clone(), event_queue);

    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    WaylandSource::new(conn.clone(), event_queue)
        .insert(loop_handle.clone())
        .expect("Failed to insert EventLoop into WaylandSource");

    let compositor = CompositorState::bind(&globals, &queue_handle)?;
    let layer_shell = LayerShell::bind(&globals, &queue_handle)?;
    let xdg_shell = XdgShell::bind(&globals, &queue_handle)?;

    // Init WGPU
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::from_env_or_default());

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

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        compatible_surface:     Some(&dummy_surface_wgpu),
        power_preference:       wgpu::PowerPreference::LowPower,
        force_fallback_adapter: false,
    }))?;
    dummy_surface.destroy();

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label:             Some("vello_device"),
        required_features: wgpu::Features::empty(),
        required_limits:   wgpu::Limits::default(),
        memory_hints:      wgpu::MemoryHints::default(),
        trace:             wgpu::Trace::Off,
    }))?;

    let renderer = vello::Renderer::new(&device, vello::RendererOptions {
        use_cpu:              false,
        antialiasing_support: vello::AaSupport::all(),
        num_init_threads:     None,
        pipeline_cache:       None, // TODO: Investigate
    })?;

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

        instance,
        adapter,
        device,
        queue,

        renderer,
        font_context: parley::FontContext::new(),
        layout_context: parley::LayoutContext::new(),

        displays: HashMap::new(),
        surfaces: HashMap::new(),
        program_builder: Box::new(move || Box::new(program_builder())),
    };

    loop {
        event_loop.dispatch(None, &mut app)?;
        //debug!("new dispatch?");
    }
}

struct App {
    loop_handle: LoopHandle<'static, Self>,

    registry_state: RegistryState,
    seat_state:     SeatState,
    output_state:   OutputState,

    fractional_manager: WpFractionalScaleManagerV1,
    pointers:           HashMap<WlSeat, WlPointer>,

    compositor:  CompositorState,
    layer_shell: LayerShell,
    xdg_shell:   XdgShell,

    instance: wgpu::Instance,
    adapter:  wgpu::Adapter,
    device:   wgpu::Device,
    queue:    wgpu::Queue,

    renderer:       Renderer,
    font_context:   parley::FontContext,
    layout_context: parley::LayoutContext<Brush>,

    displays: HashMap<WlOutput, Display>,  // WlOutput
    surfaces: HashMap<WlSurface, Surface>, // WlSurface

    program_builder: Box<dyn Fn() -> Box<dyn Program>>,
}

struct Display {
    _layer_root: LayerSurface,
    last_time:   Option<Duration>,

    scale:    f64,
    surfaces: HashMap<WlSurface, wgpu::Surface<'static>>, // WlSurface
}

impl App {
    pub fn new_display(
        &self,
        conn: &Connection,
        layer: LayerSurface,
        _qh: &QueueHandle<Self>,
    ) -> Display {
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
                self.instance
                    .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                        raw_display_handle: display_handle,
                        raw_window_handle:  surface_handle,
                    })
            }
            .expect("Failed to create Wgpu Surface")
        };

        #[allow(clippy::mutable_key_type)]
        let mut surfaces = HashMap::new();
        surfaces.insert(layer.wl_surface().clone(), surface_wgpu);

        Display {
            _layer_root: layer,
            last_time: None,

            scale: 1.0,
            surfaces,
        }
    }
}

struct Surface {
    output: WlOutput,

    size:     (u32, u32),
    callback: Box<dyn Program>,

    input_events: Vec<InputEvent>,
    _fractional:  WpFractionalScaleV1,
}

impl App {
    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    fn draw(
        &mut self,
        surface: &WlSurface,
        qh: &QueueHandle<Self>,
    ) {
        let Some(info) = self.surfaces.get_mut(surface) else {
            warn!("Attempted draw on unregistered WlSurface");
            return;
        };

        let Some(display) = self.displays.get(&info.output) else {
            warn!("Attempted draw on unregistered WlOutput");
            return;
        };

        let mut events = Vec::new();
        std::mem::swap(&mut info.input_events, &mut events);

        let (width, height) = info.size;
        if width == 0 || height == 0 {
            warn!("Attempted render on unconfigured WlSurface");
            return;
        }

        let mut context = RenderContext {
            scene:  vello::Scene::new(),
            events: &events,

            viewport_info: crate::ViewportInfo::from_pixel_size(width, height, display.scale),

            font_context:   &mut self.font_context,
            layout_context: &mut self.layout_context,

            requested_redraw: None,
            current_time:     display.last_time.unwrap_or_default(),
        };

        info.callback.draw(&mut context);

        if let Some(redraw) = context.requested_redraw {
            let surface = surface.clone();
            let queue_handle = qh.clone();

            _ = self
                .loop_handle
                .insert_source(Timer::from_duration(redraw), move |_, (), ctx| {
                    if ctx.surfaces.contains_key(&surface) {
                        surface.frame(&queue_handle, surface.clone());
                        surface.commit();
                    }

                    TimeoutAction::Drop
                });
        }

        let RenderContext { scene, .. } = context;

        let Some(display) = self.displays.get(&info.output) else {
            warn!("Unregistered WlOutput");
            return;
        };

        let Some(surface_wgpu) = display.surfaces.get(surface) else {
            warn!("WlSurface not Configured");
            return;
        };
        let surface_texture = surface_wgpu
            .get_current_texture()
            .expect("Failed to get SurfaceTexture");

        let render_texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label:           Some("vello_render_texture"),
            size:            wgpu::Extent3d {
                depth_or_array_layers: 1,
                width,
                height,
            },
            mip_level_count: 1,
            sample_count:    1,
            dimension:       wgpu::TextureDimension::D2,
            format:          wgpu::TextureFormat::Rgba8Unorm,
            usage:           wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats:    &[wgpu::TextureFormat::Rgba8Unorm],
        });

        let texture_view = render_texture.create_view(&wgpu::TextureViewDescriptor::default());
        if self
            .renderer
            .render_to_texture(
                &self.device,
                &self.queue,
                &scene,
                &texture_view,
                &vello::RenderParams {
                    base_color: palette::css::TRANSPARENT,
                    width,
                    height,
                    antialiasing_method: vello::AaConfig::Msaa16,
                },
            )
            .is_err()
        {
            tracing::error!("Failed to render!");
            return;
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vello_encoder"),
            });

        let blit =
            wgpu::util::TextureBlitterBuilder::new(&self.device, surface_texture.texture.format())
                .blend_state(wgpu::BlendState::ALPHA_BLENDING)
                .build();
        blit.copy(
            &self.device,
            &mut encoder,
            &texture_view,
            &surface_texture
                .texture
                .create_view(&wgpu::TextureViewDescriptor {
                    label: Some("surface_texture"),
                    //usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
                    ..Default::default()
                }),
        );

        self.queue.submit(std::iter::once(encoder.finish()));
        surface_texture.present();
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

        display.scale = f64::from(new_factor);
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
        let time = Duration::from_millis(u64::from(time));

        let Some(info) = self.surfaces.get(surface) else {
            warn!("Unassigned WlSurface");
            return;
        };

        {
            let output = info.output.clone();
            let Some(display) = self.displays.get_mut(&output) else {
                warn!("Unassigned WlSurface");
                return;
            };

            display.last_time = Some(time);
        }

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
            output:   output.clone(),
            callback: (self.program_builder)(),
            size:     (40, 0),

            input_events: Vec::new(),
            _fractional:  fractional,
        });

        self.displays
            .insert(output, self.new_display(conn, layer, qh));
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
        let Some(display) = self.displays.remove(&output) else {
            return;
        };

        for id in display.surfaces.keys() {
            self.surfaces.remove(id);
        }
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

impl PointerHandler for App {
    #[allow(clippy::cast_possible_truncation)]
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        for PointerEvent {
            surface,
            position: (pos_x, pos_y),
            kind,
        } in events
        {
            let Some(info) = self.surfaces.get_mut(surface) else {
                warn!("Unregistered WlSurface");
                continue;
            };

            let (x, y) = (*pos_x, *pos_y);
            let event = match kind {
                PointerEventKind::Enter { serial: _ } => InputEvent::PointerEnter { x, y },
                PointerEventKind::Leave { serial: _ } => InputEvent::PointerLeave,

                PointerEventKind::Press {
                    time,
                    button,
                    serial: _,
                } => InputEvent::PointerButton {
                    time:   Duration::from_millis(u64::from(*time)),
                    button: *button,
                    state:  true,
                },
                PointerEventKind::Release {
                    time,
                    button,
                    serial: _,
                } => InputEvent::PointerButton {
                    time:   Duration::from_millis(u64::from(*time)),
                    button: *button,
                    state:  false,
                },

                PointerEventKind::Motion { time } => InputEvent::PointerMove {
                    time: Duration::from_millis(u64::from(*time)),
                    x,
                    y,
                },
                PointerEventKind::Axis {
                    time,
                    horizontal,
                    vertical,
                    source: _,
                } => {
                    let horizontal = f64::from(horizontal.value120) / 120.0;
                    let vertical = f64::from(vertical.value120) / 120.0;

                    InputEvent::PointerAxis {
                        time: Duration::from_millis(u64::from(*time)),
                        horizontal,
                        vertical,
                    }
                },
            };
            info.input_events.push(event);

            // Request frame
            surface.frame(qh, surface.clone());
            surface.commit();
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
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(info) = self.surfaces.get_mut(layer.wl_surface()) else {
            warn!("Unregistered WlSurface");
            return;
        };

        let Some(display) = self.displays.get_mut(&info.output) else {
            warn!("WlSurface attached to Unregistered WlOutput");
            return;
        };

        let Some(surface_wgpu) = display.surfaces.get(layer.wl_surface()) else {
            warn!("WGPU surface not created!");
            return;
        };

        let (width, height) = configure.new_size;

        let cap = surface_wgpu.get_capabilities(&self.adapter);
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: cap.formats[0],
            view_formats: vec![cap.formats[0]],
            alpha_mode: wgpu::CompositeAlphaMode::PreMultiplied,
            width,
            height,
            desired_maximum_frame_latency: 2,
            // Wayland is inherently a mailbox system.
            present_mode: wgpu::PresentMode::Mailbox,
        };
        surface_wgpu.configure(&self.device, &surface_config);
        info.size = configure.new_size;

        debug!(id = ?layer.wl_surface().id(), width, height, "Configured");

        self.draw(layer.wl_surface(), qh);
        layer.wl_surface().frame(qh, layer.wl_surface().clone());
        layer.wl_surface().commit();
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

                let scale = f64::from(scale) / 120.0;
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
