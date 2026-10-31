//! Wayland 原生左上角覆盖层；不占工作区、不抢键盘、不截获鼠标。

use super::{DURATION, HEIGHT, MARGIN, WIDTH};
use crate::ModeOsd;
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Platform, WindowAdapter, WindowEvent};
use slint::ComponentHandle;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::os::fd::{AsFd, AsRawFd};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Instant;
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};

pub enum Command {
    Show(&'static str),
    Quit,
}

struct Software {
    window: Rc<MinimalSoftwareWindow>,
}
impl Platform for Software {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

struct Osd {
    registry: RegistryState,
    output: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: Option<LayerSurface>,
    configured: bool,
    deadline: Option<Instant>,
    window: Rc<MinimalSoftwareWindow>,
    ui: ModeOsd,
    error: Option<String>,
}

impl Osd {
    fn hide(&mut self) {
        if let Some(layer) = self.layer.take() {
            layer.wl_surface().attach(None, 0, 0);
            layer.commit();
        }
        self.configured = false;
        self.deadline = None;
    }
    fn draw(&mut self) -> Result<(), String> {
        if !self.configured {
            return Ok(());
        }
        let Some(layer) = &self.layer else {
            return Ok(());
        };
        // 2x 软件缓冲在高 DPI 屏幕上保留清晰字体，逻辑尺寸与边距不变。
        let width = WIDTH as usize * 2;
        let height = HEIGHT as usize * 2;
        let mut pixels = vec![PremultipliedRgbaColor::default(); width * height];
        self.window.request_redraw();
        self.window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, width);
        });
        let (buffer, canvas) = self
            .pool
            .create_buffer(
                width as i32,
                height as i32,
                (width * 4) as i32,
                wl_shm::Format::Argb8888,
            )
            .map_err(|e| e.to_string())?;
        for (pixel, target) in pixels.iter().zip(canvas.chunks_exact_mut(4)) {
            target.copy_from_slice(
                &u32::from_be_bytes([pixel.alpha, pixel.red, pixel.green, pixel.blue])
                    .to_ne_bytes(),
            );
        }
        layer
            .wl_surface()
            .damage_buffer(0, 0, width as i32, height as i32);
        buffer
            .attach_to(layer.wl_surface())
            .map_err(|e| e.to_string())?;
        layer.commit();
        Ok(())
    }
}

pub fn run(commands: Receiver<Command>) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, mut queue) = registry_queue_init::<Osd>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).map_err(|e| e.to_string())?;
    let shell = LayerShell::bind(&globals, &qh).map_err(|e| e.to_string())?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| e.to_string())?;
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    // 独立线程的纯软件平台不安装全局事件代理，不影响主 GUI 的 winit 事件循环。
    slint::platform::set_platform(Box::new(Software {
        window: window.clone(),
    }))
    .map_err(|e| e.to_string())?;
    let ui = ModeOsd::new().map_err(|e| e.to_string())?;
    window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: 2.0 });
    window.set_size(slint::PhysicalSize::new(WIDTH * 2, HEIGHT * 2));
    ui.show().map_err(|e| e.to_string())?;
    let pool = SlotPool::new((WIDTH * HEIGHT * 16) as usize, &shm).map_err(|e| e.to_string())?;
    let mut osd = Osd {
        registry: RegistryState::new(&globals),
        output: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer: None,
        configured: false,
        deadline: None,
        window,
        ui,
        error: None,
    };
    loop {
        queue
            .dispatch_pending(&mut osd)
            .map_err(|e| e.to_string())?;
        if let Some(error) = osd.error.take() {
            return Err(error);
        }
        // 丢弃积压的中间挡位，只显示最新挡位。
        let mut latest = None;
        loop {
            match commands.try_recv() {
                Ok(Command::Show(text)) => latest = Some(text),
                Ok(Command::Quit) | Err(TryRecvError::Disconnected) => {
                    osd.hide();
                    return Ok(());
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if let Some(text) = latest {
            osd.ui.set_mode_label(text.into());
            osd.deadline = Some(Instant::now() + DURATION);
            if osd.layer.is_none() {
                let surface = compositor.create_surface(&qh);
                let region = Region::new(&compositor).map_err(|e| e.to_string())?;
                surface.set_input_region(Some(region.wl_region()));
                surface.set_buffer_scale(2);
                let layer = shell.create_layer_surface(
                    &qh,
                    surface,
                    Layer::Overlay,
                    Some("firebat-mode-osd"),
                    None,
                );
                layer.set_anchor(Anchor::TOP | Anchor::LEFT);
                layer.set_margin(MARGIN, 0, 0, MARGIN);
                layer.set_size(WIDTH, HEIGHT);
                layer.set_keyboard_interactivity(KeyboardInteractivity::None);
                layer.set_exclusive_zone(-1);
                layer.commit();
                osd.layer = Some(layer);
            } else {
                osd.draw()?;
            }
        }
        if osd
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            osd.hide();
        }
        conn.flush().map_err(|e| e.to_string())?;
        // prepare_read + poll 防止阻塞，使显示/隐藏命令在 20ms 内处理。
        if let Some(guard) = queue.prepare_read() {
            let mut fd = libc::pollfd {
                fd: conn.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut fd, 1, 20) };
            if ready > 0 {
                guard.read().map_err(|e| e.to_string())?;
            } else if ready < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
    }
}

impl CompositorHandler for Osd {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}
impl LayerShellHandler for Osd {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if self.layer.as_ref().is_some_and(|current| current == layer) {
            self.error = Some("合成器关闭了挡位覆盖层".into());
        }
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        if self.layer.as_ref().is_some_and(|current| current == layer) {
            self.configured = true;
            if let Err(e) = self.draw() {
                self.error = Some(e);
            }
        }
    }
}
impl OutputHandler for Osd {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}
impl ShmHandler for Osd {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}
impl ProvidesRegistryState for Osd {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}
delegate_compositor!(Osd);
delegate_output!(Osd);
delegate_shm!(Osd);
delegate_layer!(Osd);
delegate_registry!(Osd);
