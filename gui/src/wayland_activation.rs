//! 在已有 Wayland 窗口上使用托盘提供的 xdg-activation 令牌。
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use slint::winit_030::winit::window::Window;
use wayland_client::{
    backend::{Backend, ObjectId},
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_registry, wl_surface},
    Connection, Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;

struct State;
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
wayland_client::delegate_noop!(State: ignore XdgActivationV1);

pub fn activate(window: &Window, token: &str) -> Result<(), String> {
    let display = window.display_handle().map_err(|e| e.to_string())?;
    let surface = window.window_handle().map_err(|e| e.to_string())?;
    let (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) = (display.as_raw(), surface.as_raw()) else {
        return Err("不是 Wayland 窗口".into());
    };
    // 借用的 display 和 surface 在本函数执行期间由 winit 的窗口持有，
    // 外部 backend 不接管连接，也不销毁或接管原有 surface。
    let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
    let connection = Connection::from_backend(backend);
    let id = unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface.surface.as_ptr().cast()) }
        .map_err(|e| e.to_string())?;
    let surface = wl_surface::WlSurface::from_id(&connection, id).map_err(|e| e.to_string())?;
    let (globals, queue) = registry_queue_init::<State>(&connection).map_err(|e| e.to_string())?;
    let activation: XdgActivationV1 = globals.bind(&queue.handle(), 1..=1, ()).map_err(|e| e.to_string())?;
    activation.activate(token.to_string(), &surface);
    activation.destroy();
    connection.flush().map_err(|e| e.to_string())
}
