//! 主窗口的激活与任务栏切换；Wayland 通过启动令牌恢复焦点。
use crate::AppWindow;
use slint::{winit_030::WinitWindowAccessor, ComponentHandle};
use std::cell::RefCell;

thread_local! { static TOKEN: RefCell<Option<String>> = const { RefCell::new(None) }; }

pub fn take_environment_token() -> Option<String> {
    let token = std::env::var("XDG_ACTIVATION_TOKEN")
        .ok()
        .or_else(|| std::env::var("DESKTOP_STARTUP_ID").ok());
    slint::winit_030::winit::platform::startup_notify::reset_activation_token_env();
    token
}
pub fn set_token(token: Option<String>) {
    TOKEN.with(|slot| *slot.borrow_mut() = token);
}
pub fn take_token() -> Option<String> {
    TOKEN.with(|slot| slot.borrow_mut().take())
}

pub fn open(ui: &AppWindow, token: Option<String>) {
    // 始终恢复同一个 Slint 主窗口，不为激活令牌额外 hide/show 重建可见窗口。
    use slint::winit_030::winit::platform::wayland::WindowExtWayland;
    if ui.window().is_minimized()
        && ui
            .window()
            .with_winit_window(|w| w.xdg_toplevel().is_some())
            .unwrap_or(false)
    {
        // Wayland 不支持 set_minimized(false)；先释放已最小化的原生资源再显示。
        // AppWindow 本身不变，同时仍只有一个主窗口。
        if let Err(e) = ui.hide() {
            eprintln!("恢复最小化窗口失败: {e}");
            return;
        }
    }
    set_token(token);
    ui.window().set_minimized(false);
    if let Err(e) = ui.show() {
        eprintln!("打开窗口失败: {e}");
        return;
    }
    ui.window().set_minimized(false);
    ui.window().with_winit_window(|w| {
        w.set_minimized(false);
        w.focus_window();
    });
}

pub fn toggle(ui: &AppWindow, token: Option<String>) {
    let focused = ui
        .window()
        .with_winit_window(|w| w.has_focus())
        .unwrap_or(false);
    if should_minimize(
        ui.window().is_visible(),
        ui.window().is_minimized(),
        focused,
    ) {
        ui.window().set_minimized(true);
    } else {
        open(ui, token);
    }
}

fn should_minimize(visible: bool, minimized: bool, focused: bool) -> bool {
    visible && !minimized && focused
}

#[cfg(test)]
mod tests {
    #[test]
    fn taskbar_toggle_preserves_restore_cases() {
        assert!(super::should_minimize(true, false, true));
        for state in [
            (false, false, false),
            (true, true, false),
            (true, false, false),
            (false, false, true),
        ] {
            assert!(!super::should_minimize(state.0, state.1, state.2));
        }
    }
}
