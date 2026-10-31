//! Linux 托盘；所有窗口操作都转交 Slint 主线程。

use crate::{push_log, toast, AppWindow};
use ksni::blocking::TrayMethods;
use slint::{ComponentHandle, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn open_window(ui: &AppWindow) {
    crate::window::open(ui, None);
}

struct Tray {
    ui: Weak<AppWindow>,
    available: Arc<Availability>,
    icon: ksni::Icon,
}

impl Tray {
    fn open(&self) {
        // 令牌仅在 ksni 事件上下文可读，不能到 Slint 回调中才读取。
        let mut token = ksni::current_xdg_activation_token();
        if token.is_none() && std::env::var_os("WAYLAND_DISPLAY").is_some() {
            for _ in 0..15 {
                std::thread::sleep(std::time::Duration::from_millis(1));
                token = ksni::current_xdg_activation_token();
                if token.is_some() {
                    break;
                }
            }
        }
        let weak = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                crate::window::open(&ui, token);
            }
        });
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "firebat-cc-gui".into()
    }
    fn title(&self) -> String {
        "Firebat 控制中心".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![self.icon.clone()]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Firebat 控制中心".into(),
            description: "左键打开，右键选择打开或关闭".into(),
            ..Default::default()
        }
    }
    fn activate(&mut self, _: i32, _: i32) {
        self.open();
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            ksni::menu::StandardItem {
                label: "打开".into(),
                activate: Box::new(|tray: &mut Self| tray.open()),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            ksni::menu::StandardItem {
                label: "关闭".into(),
                activate: Box::new(|_: &mut Self| {
                    // 返回 main 的正常退出路径，释放监听 owner；不可 process::exit。
                    let _ = slint::invoke_from_event_loop(|| {
                        let _ = slint::quit_event_loop();
                    });
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
    fn watcher_offline(&self, _: ksni::OfflineReason) -> bool {
        self.available.online.store(false, Ordering::Release);
        let weak = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                open_window(&ui);
                push_log(&ui, "系统托盘已断开，已恢复窗口；后台监听继续运行", 2);
                toast(&ui, "系统托盘不可用，已恢复窗口", 2);
            }
        });
        true // 桌面面板重启后允许重新注册。
    }
    fn watcher_online(&self) {
        self.available.online.store(true, Ordering::Release);
    }
}

pub struct Availability {
    registered: AtomicBool,
    online: AtomicBool,
}

impl Availability {
    fn new() -> Self {
        Self {
            registered: AtomicBool::new(false),
            online: AtomicBool::new(true),
        }
    }
    fn usable(&self) -> bool {
        self.registered.load(Ordering::Acquire) && self.online.load(Ordering::Acquire)
    }
}

pub struct Controller {
    available: Arc<Availability>,
    stopped: Arc<AtomicBool>,
    handle: Arc<Mutex<Option<ksni::blocking::Handle<Tray>>>>,
}

impl Controller {
    pub fn start(ui: &AppWindow) -> Self {
        let controller = Self {
            available: Arc::new(Availability::new()),
            stopped: Arc::new(AtomicBool::new(false)),
            handle: Arc::new(Mutex::new(None)),
        };
        let available = controller.available.clone();
        let stopped = controller.stopped.clone();
        let handle = controller.handle.clone();
        let weak = ui.as_weak();
        // 原 Logo 编译期嵌入；RGBA 转 SNI 要求的网络字节序 ARGB。
        let icon =
            slint::Image::load_from_data(include_bytes!("../ui/assets/logo.png"), Some("png"))
                .ok()
                .and_then(|image| image.to_rgba8())
                .map(|pixels| {
                    let mut data = pixels.as_bytes().to_vec();
                    for pixel in data.chunks_exact_mut(4) {
                        pixel.rotate_right(1);
                    }
                    ksni::Icon {
                        width: pixels.width() as i32,
                        height: pixels.height() as i32,
                        data,
                    }
                });
        std::thread::spawn(move || {
            let result = icon
                .ok_or_else(|| "无法读取托盘图标".to_string())
                .and_then(|icon| {
                    // spawn 成功即表示已注册到真实 watcher；失败时不启用隐藏窗口。
                    Tray {
                        ui: weak.clone(),
                        available: available.clone(),
                        icon,
                    }
                    .spawn()
                    .map_err(|e| e.to_string())
                });
            let mut slot = handle.lock().unwrap();
            match result {
                Ok(tray) if !stopped.load(Ordering::Acquire) => {
                    // 注册状态与在线状态分开，不能覆盖同时到达的 offline 通知。
                    available.registered.store(true, Ordering::Release);
                    *slot = Some(tray);
                }
                Ok(tray) => {
                    tray.shutdown();
                }
                Err(e) => {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            push_log(&ui, format!("系统托盘不可用：{e}；关闭窗口将退出 GUI"), 2);
                        }
                    });
                }
            }
        });
        controller
    }

    pub fn available(&self) -> Arc<Availability> {
        self.available.clone()
    }
}

impl Drop for Controller {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(handle) = self.handle.lock().unwrap().take() {
            handle.shutdown();
        }
    }
}

pub fn install_close_handler(ui: &AppWindow, available: Arc<Availability>) {
    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        if available.usable() {
            if let Some(ui) = weak.upgrade() {
                match ui.hide() {
                    Ok(()) => push_log(&ui, "已进入托盘，性能模式键监听保持运行", 0),
                    Err(e) => toast(&ui, format!("隐藏窗口失败：{e}"), 3),
                }
            }
        } else {
            // 托盘不可用时保留原关闭行为，避免留下无法找回的后台窗口。
            let _ = slint::quit_event_loop();
        }
        slint::CloseRequestResponse::KeepWindowShown
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ksni::Tray as _;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{EventLoopProxy, Platform, WindowAdapter, WindowEvent};
    use std::collections::VecDeque;
    use std::rc::Rc;

    type Queue = Arc<Mutex<VecDeque<Box<dyn FnOnce() + Send>>>>;
    #[derive(Clone)]
    struct Events {
        queue: Queue,
        quit: Arc<AtomicBool>,
    }
    impl EventLoopProxy for Events {
        fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
            self.quit.store(true, Ordering::Relaxed);
            Ok(())
        }
        fn invoke_from_event_loop(
            &self,
            event: Box<dyn FnOnce() + Send>,
        ) -> Result<(), slint::EventLoopError> {
            self.queue.lock().unwrap().push_back(event);
            Ok(())
        }
    }
    impl Events {
        fn drain(&self) {
            loop {
                let event = self.queue.lock().unwrap().pop_front();
                let Some(event) = event else { break };
                event();
            }
        }
    }
    struct Headless {
        window: Rc<MinimalSoftwareWindow>,
        events: Events,
        created: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Platform for Headless {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            self.created.fetch_add(1, Ordering::Relaxed);
            Ok(self.window.clone())
        }
        fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
            Some(Box::new(self.events.clone()))
        }
        fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
            Ok(()) // 测试由 Events::drain 手动推进事件，不阻塞。
        }
    }

    #[test]
    fn close_restore_exit_and_lost_tray() {
        let events = Events {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            quit: Arc::new(AtomicBool::new(false)),
        };
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        let created = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        slint::platform::set_platform(Box::new(Headless {
            window,
            events: events.clone(),
            created: created.clone(),
        }))
        .unwrap();
        let ui = AppWindow::new().unwrap();
        ui.global::<crate::Backend>().set_listening(true);
        ui.global::<crate::Nav>().set_page(3);
        let available = Arc::new(Availability::new());
        available.registered.store(true, Ordering::Release);
        install_close_handler(&ui, available.clone());
        let mut tray = Tray {
            ui: ui.as_weak(),
            available,
            icon: ksni::Icon {
                width: 1,
                height: 1,
                data: vec![255; 4],
            },
        };
        ui.show().unwrap();
        slint::run_event_loop_until_quit().unwrap();
        ui.window().dispatch_event(WindowEvent::CloseRequested);
        assert!(!ui.window().is_visible());
        assert!(!events.quit.load(Ordering::Relaxed));
        assert!(ui.global::<crate::Backend>().get_listening());
        tray.activate(0, 0); // 左键恢复。
        events.drain();
        assert!(ui.window().is_visible());
        for _ in 0..3 {
            ui.window().dispatch_event(WindowEvent::CloseRequested);
            assert!(!ui.window().is_visible());
            crate::window::open(&ui, Some("test-activation-token".into()));
            assert!(ui.window().is_visible());
            assert_eq!(ui.global::<crate::Nav>().get_page(), 3);
            assert!(ui.global::<crate::Backend>().get_listening());
            assert_eq!(
                created.load(Ordering::Relaxed),
                1,
                "恢复必须复用原主窗口组件"
            );
        }
        ui.hide().unwrap();
        if let ksni::MenuItem::Standard(item) = &tray.menu()[0] {
            (item.activate)(&mut tray);
        } else {
            panic!("缺少打开菜单");
        }
        events.drain();
        assert!(ui.window().is_visible());
        ui.hide().unwrap();
        tray.watcher_offline(ksni::OfflineReason::No);
        events.drain();
        assert!(ui.window().is_visible());
        assert!(!tray.available.usable());
        tray.watcher_online();
        assert!(tray.available.usable());
        if let ksni::MenuItem::Standard(item) = &tray.menu()[2] {
            (item.activate)(&mut tray);
        } else {
            panic!("缺少关闭菜单");
        }
        events.drain();
        assert!(events.quit.swap(false, Ordering::Relaxed));
        tray.available.online.store(false, Ordering::Release);
        ui.window().dispatch_event(WindowEvent::CloseRequested);
        assert!(events.quit.load(Ordering::Relaxed));
    }
}
