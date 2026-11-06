//! 性能挡位 OSD：X11 无焦点通知窗口，Wayland 使用 layer-shell。

mod wayland;

use crate::{AppWindow, ModeOsd};
use slint::{ComponentHandle, Timer, TimerMode, Weak};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const WIDTH: u32 = 280;
pub const HEIGHT: u32 = 96;
pub const MARGIN: i32 = 32;
pub const DURATION: Duration = Duration::from_secs(5);

thread_local! {
    static OSD: RefCell<Option<Controller>> = const { RefCell::new(None) };
    static CREATING_OSD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn label(mode: i32) -> Option<&'static str> {
    match mode {
        1 | 4 => Some("office"),
        2 => Some("game"),
        3 => Some("turbo"),
        _ => None,
    }
}

pub fn install_backend() -> Result<(), slint::PlatformError> {
    use slint::winit_030::winit::{
        platform::startup_notify::WindowAttributesExtStartupNotify,
        platform::wayland::WindowAttributesExtWayland,
        platform::x11::{WindowAttributesExtX11, WindowType},
        window::WindowLevel,
    };
    slint::BackendSelector::new()
        .with_winit_window_attributes_hook(|attrs| {
            // 属性钩子发生在 Slint 应用组件 title 之前，不能按 attrs.title 判断角色。
            let osd = CREATING_OSD.with(|role| role.get());
            let app_id = if osd {
                "firebat-cc-gui-osd"
            } else {
                crate::desktop::APP_ID
            };
            let attrs = WindowAttributesExtWayland::with_name(attrs, app_id, app_id);
            let attrs = WindowAttributesExtX11::with_name(attrs, app_id, app_id);
            if osd {
                attrs
                    .with_active(false)
                    .with_decorations(false)
                    .with_resizable(false)
                    .with_transparent(true)
                    .with_window_level(WindowLevel::AlwaysOnTop)
                    .with_x11_window_type(vec![WindowType::Notification])
            } else {
                let attrs = attrs.with_active(true);
                match crate::window::take_token() {
                    Some(token) => attrs.with_activation_token(
                        slint::winit_030::winit::window::ActivationToken::from_raw(token),
                    ),
                    None => attrs,
                }
            }
        })
        .select()?;
    // Slint 在 Wayland hide/show 后重建原生窗口时会重新读取此上下文 ID；
    // 初次创建 adapter 的属性钩子不会再次执行。
    slint::set_xdg_app_id(crate::desktop::APP_ID)
}

struct Native {
    window: ModeOsd,
    timer: Timer,
    main: Weak<AppWindow>,
}

impl Native {
    fn show(&self, text: &str, duration: Duration) {
        use slint::winit_030::{
            winit::{dpi::PhysicalPosition, window::WindowLevel},
            WinitWindowAccessor,
        };
        // 选择主窗口所在屏幕；主窗口在托盘时回退到首选屏幕。
        let monitor = self.main.upgrade().and_then(|ui| {
            ui.window()
                .with_winit_window(|w| w.current_monitor().or_else(|| w.primary_monitor()))
                .flatten()
        });
        self.window.set_mode_label(text.into());
        let (x, y) = monitor
            .as_ref()
            .map(|monitor| {
                let origin = monitor.position();
                let margin = (MARGIN as f64 * monitor.scale_factor()).round() as i32;
                (origin.x + margin, origin.y + margin)
            })
            .unwrap_or((MARGIN, MARGIN));
        self.window
            .window()
            .set_position(slint::PhysicalPosition::new(x, y));
        let _ = slint::set_xdg_app_id("firebat-cc-gui-osd");
        let shown = self.window.show();
        let _ = slint::set_xdg_app_id(crate::desktop::APP_ID);
        if let Err(e) = shown {
            eprintln!("显示挡位提示失败: {e}");
            return;
        }
        self.window.window().with_winit_window(|window| {
            let monitor = monitor
                .or_else(|| window.current_monitor())
                .or_else(|| window.primary_monitor());
            if let Some(monitor) = monitor {
                let origin = monitor.position();
                let margin = (MARGIN as f64 * monitor.scale_factor()).round() as i32;
                window.set_outer_position(PhysicalPosition::new(
                    origin.x + margin,
                    origin.y + margin,
                ));
            }
            window.set_window_level(WindowLevel::AlwaysOnTop);
            let _ = window.set_cursor_hittest(false);
        });
        let weak = self.window.as_weak();
        self.timer.start(TimerMode::SingleShot, duration, move || {
            if let Some(window) = weak.upgrade() {
                let _ = window.hide();
            }
        });
    }
}

struct Controller {
    last: Option<&'static str>,
    active: Option<(&'static str, Instant)>,
    native: Option<Rc<Native>>,
    main: Weak<AppWindow>,
    layer: Option<mpsc::Sender<wayland::Command>>,
}

impl Drop for Controller {
    fn drop(&mut self) {
        if let Some(native) = &self.native {
            native.timer.stop();
            let _ = native.window.hide();
        }
        if let Some(layer) = &self.layer {
            let _ = layer.send(wayland::Command::Quit);
        }
    }
}

pub fn init(main: &AppWindow) -> Result<(), slint::PlatformError> {
    let layer = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let (tx, rx) = mpsc::channel();
        // 初始化或运行中 layer-shell 失败时交回 Slint 窗口；失败只记录一次。
        let weak = main.as_weak();
        std::thread::spawn(move || {
            if let Err(e) = wayland::run(rx) {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        crate::push_log(
                            &ui,
                            format!("Wayland 覆盖层不可用：{e}；使用普通提示窗口，位置由桌面决定"),
                            2,
                        );
                    }
                    OSD.with(|osd| {
                        if let Some(osd) = osd.borrow_mut().as_mut() {
                            osd.layer = None;
                            if let Some((text, deadline)) = osd.active {
                                let remaining = deadline.saturating_duration_since(Instant::now());
                                if !remaining.is_zero() {
                                    osd.show_native(text, remaining);
                                }
                            }
                        }
                    });
                });
            }
        });
        Some(tx)
    } else {
        None
    };
    OSD.with(|slot| *slot.borrow_mut() = Some(Controller::new(main.as_weak(), layer)));
    Ok(())
}

fn update(mode: i32, confirmed: bool) {
    let Some(text) = label(mode) else { return };
    OSD.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(osd) = slot.as_mut() else { return };
        let show = confirmed || osd.last.is_some_and(|last| last != text);
        osd.last = Some(text);
        if show {
            osd.active = Some((text, Instant::now() + DURATION));
            if let Some(layer) = &osd.layer {
                if layer.send(wayland::Command::Show(text)).is_ok() {
                    return;
                }
                osd.layer = None;
            }
            osd.show_native(text, DURATION);
        }
    });
}

impl Controller {
    fn new(main: Weak<AppWindow>, layer: Option<mpsc::Sender<wayland::Command>>) -> Self {
        Self {
            last: None,
            active: None,
            native: None,
            main,
            layer,
        }
    }
    fn show_native(&mut self, text: &str, duration: Duration) {
        if self.native.is_none() {
            let _ = slint::set_xdg_app_id("firebat-cc-gui-osd");
            CREATING_OSD.with(|role| role.set(true));
            let window = ModeOsd::new();
            CREATING_OSD.with(|role| role.set(false));
            let _ = slint::set_xdg_app_id(crate::desktop::APP_ID);
            match window {
                Ok(window) => {
                    self.native = Some(Rc::new(Native {
                        window,
                        timer: Timer::default(),
                        main: self.main.clone(),
                    }))
                }
                Err(e) => {
                    eprintln!("创建挡位提示失败: {e}");
                    return;
                }
            }
        }
        self.native.as_ref().unwrap().show(text, duration);
    }
}

/// 首次读取只建立基线，之后轮询检测到外部切换时显示。
pub fn observe(mode: i32) {
    update(mode, false);
}
/// GUI 写入成功或收到模式键事件后立即显示，并同步去重基线。
pub fn confirm(mode: i32) {
    update(mode, true);
}
pub fn shutdown() {
    OSD.with(|slot| {
        slot.borrow_mut().take();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{Platform, WindowAdapter};
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };

    struct Headless {
        time: Arc<AtomicU64>,
        created: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Platform for Headless {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            self.created.fetch_add(1, Ordering::Relaxed);
            Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
        }
        fn duration_since_start(&self) -> Duration {
            Duration::from_millis(self.time.load(Ordering::Relaxed))
        }
    }

    #[test]
    fn mode_changes_render_and_restart_timeout() {
        let time = Arc::new(AtomicU64::new(0));
        let created = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        slint::platform::set_platform(Box::new(Headless {
            time: time.clone(),
            created: created.clone(),
        }))
        .unwrap();
        let main = AppWindow::new().unwrap();
        assert!(!main.global::<crate::Nav>().get_ec_debug_enabled());
        OSD.with(|slot| *slot.borrow_mut() = Some(Controller::new(main.as_weak(), None)));
        observe(1);
        OSD.with(|slot| {
            assert!(
                slot.borrow().as_ref().unwrap().native.is_none(),
                "启动和首次读取不能创建额外窗口"
            )
        });
        assert_eq!(created.load(Ordering::Relaxed), 1);
        confirm(2);
        let native = OSD.with(|slot| {
            slot.borrow()
                .as_ref()
                .unwrap()
                .native
                .as_ref()
                .unwrap()
                .clone()
        });
        native
            .window
            .window()
            .set_size(slint::PhysicalSize::new(WIDTH, HEIGHT));
        assert_eq!(
            created.load(Ordering::Relaxed),
            2,
            "仅切换挡位时创建小型提示窗口"
        );
        assert!(native.window.window().is_visible());
        assert_eq!(native.window.get_mode_label(), "game");
        assert!(!main.window().is_visible(), "提示不能打开托盘中的主窗口");
        time.store(4000, Ordering::Relaxed);
        slint::platform::update_timers_and_animations();
        confirm(3);
        assert_eq!(native.window.get_mode_label(), "turbo");
        time.store(5001, Ordering::Relaxed);
        slint::platform::update_timers_and_animations();
        assert!(native.window.window().is_visible(), "快速切换须重新计时");
        time.store(8500, Ordering::Relaxed);
        slint::platform::update_timers_and_animations();
        observe(3); // 重复轮询不能延长展示时间。
        time.store(9001, Ordering::Relaxed);
        slint::platform::update_timers_and_animations();
        assert!(!native.window.window().is_visible());
        observe(1); // 轮询检测到的外部切换也需要提示。
        assert!(native.window.window().is_visible());
        assert_eq!(native.window.get_mode_label(), "office");
        for (mode, text) in [(1, "office"), (2, "game"), (3, "turbo")] {
            confirm(mode);
            assert_eq!(native.window.get_mode_label(), text);
            let image = native.window.window().take_snapshot().unwrap();
            assert_eq!((image.width(), image.height()), (WIDTH, HEIGHT));
            assert_eq!(image.as_slice()[0].a, 0, "圆角外须透明");
            assert!(
                image
                    .as_slice()
                    .iter()
                    .any(|p| p.a == 179 && p.r == 0 && p.g == 0 && p.b == 0),
                "须有半透明黑底"
            );
            assert!(
                image
                    .as_slice()
                    .iter()
                    .any(|p| p.a == 255 && p.r == 255 && p.g == 255 && p.b == 255),
                "须有不透明白字"
            );
            if let Some(dir) = std::env::var_os("FIREBAT_OSD_PREVIEW_DIR") {
                let path = std::path::Path::new(&dir).join(format!("osd-{text}.rgba"));
                std::fs::write(path, image.as_bytes()).unwrap();
            }
        }
        confirm(4);
        assert_eq!(native.window.get_mode_label(), "office");
        shutdown();
        assert!(!native.window.window().is_visible());
    }
}
