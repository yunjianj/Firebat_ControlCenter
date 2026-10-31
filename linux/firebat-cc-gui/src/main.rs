//! Firebat GX 控制中心 —— Slint 图形前端
//!
//! 前端不直接碰硬件：它通过 Unix domain socket 与 `firebat-cc serve` 守护进程
//! 通信（协议见 `linux/serve-协议.md`），因此 GUI 进程**不需要 root**。
//!
//! 用法：
//!   sudo modprobe ec_sys write_support=1
//!   sudo ./firebat-cc serve
//!   ./firebat-cc-gui [--socket /run/firebat-cc.sock]

slint::include_modules!();

mod client;

use client::Client;
use serde_json::{Map, Value};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};
use std::os::raw::c_int;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- 小工具

#[repr(C)]
struct Tm {
    tm_sec: c_int,
    tm_min: c_int,
    tm_hour: c_int,
    tm_mday: c_int,
    tm_mon: c_int,
    tm_year: c_int,
    tm_wday: c_int,
    tm_yday: c_int,
    tm_isdst: c_int,
    tm_gmtoff: i64,
    tm_zone: *const i8,
}

extern "C" {
    fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
}

fn now_hm() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut tm = Tm {
        tm_sec: 0,
        tm_min: 0,
        tm_hour: 0,
        tm_mday: 0,
        tm_mon: 0,
        tm_year: 0,
        tm_wday: 0,
        tm_yday: 0,
        tm_isdst: 0,
        tm_gmtoff: 0,
        tm_zone: std::ptr::null(),
    };
    let ok = unsafe { !localtime_r(&secs, &mut tm).is_null() };
    if ok {
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    } else {
        "--:--".to_string()
    }
}

fn mode_name(idx: i32) -> &'static str {
    match idx {
        1 => "办公",
        2 => "游戏(均衡)",
        3 => "狂暴",
        4 => "自定义",
        _ => "未知",
    }
}

fn params1(k: &str, v: impl ToString) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert(k.to_string(), Value::String(v.to_string()));
    m
}

fn params2(k1: &str, v1: impl ToString, k2: &str, v2: impl ToString) -> Map<String, Value> {
    let mut m = params1(k1, v1);
    m.insert(k2.to_string(), Value::String(v2.to_string()));
    m
}

fn push_log(ui: &AppWindow, text: impl Into<SharedString>, kind: i32) {
    let b = ui.global::<Backend>();
    let model = b.get_log();
    let mut items: Vec<LogEntry> = (0..model.row_count())
        .filter_map(|i| model.row_data(i))
        .collect();
    items.insert(
        0,
        LogEntry {
            time: now_hm().into(),
            text: text.into(),
            kind,
        },
    );
    items.truncate(60);
    b.set_log(ModelRc::from(Rc::new(VecModel::from(items))));
}

fn toast(ui: &AppWindow, text: impl Into<SharedString>, kind: i32) {
    let b = ui.global::<Backend>();
    b.set_toast_text(text.into());
    b.set_toast_kind(kind);
    b.set_toast_visible(true);
}

fn run_cmd<F>(
    weak: Weak<AppWindow>,
    cli: Arc<Client>,
    cmd: &'static str,
    params: Map<String, Value>,
    done: F,
) where
    F: FnOnce(&AppWindow, Result<Value, String>) + Send + 'static,
{
    std::thread::spawn(move || {
        let result = cli.call(cmd, params);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                done(&ui, result);
            }
        });
    });
}

// ---------------------------------------------------------------- 状态应用

fn apply_status(ui: &AppWindow, data: &Value) {
    let b = ui.global::<Backend>();
    let idx = data.get("mode").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    let name = data
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("未知")
        .to_string();
    let bits = data
        .get("bits")
        .and_then(|v| v.as_str())
        .unwrap_or("--")
        .to_string();
    let backend = data
        .get("backend")
        .and_then(|v| v.as_str())
        .unwrap_or("--")
        .to_string();
    let detail = data
        .get("detail")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let raw = u8::from_str_radix(bits.trim(), 16)
        .map(|v| v as i32)
        .unwrap_or(-1);

    b.set_mode_index(idx);
    b.set_mode_name(name.into());
    b.set_mode_bits(bits.into());
    b.set_mode_raw(raw);
    b.set_fan_boost(raw >= 0 && ((raw as u8) & 0x40) != 0);
    b.set_backend_name(backend.into());
    b.set_status_detail(detail.into());
}

fn apply_gpu(ui: &AppWindow, data: &Value) {
    let b = ui.global::<Backend>();
    b.set_gpu_var(
        data.get("var")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
    );
    b.set_gpu_raw(
        data.get("mux_raw")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(-1),
    );
    b.set_gpu_state(
        data.get("interpreted")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .into(),
    );
    b.set_gpu_payload_len(
        data.get("payload_len")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(0),
    );
}

static PICKER_INIT: AtomicBool = AtomicBool::new(false);
/// 置位后下一次收到 kb status 时强制回填取色器（用于「重置背光」等外部改色操作）。
/// 平时不能每次轮询都回填，否则拖动取色器时会被 4 秒轮询覆盖。
static PICKER_RESYNC: AtomicBool = AtomicBool::new(false);

fn apply_kb(ui: &AppWindow, data: &Value) {
    let b = ui.global::<Backend>();
    b.set_kb_white_only(
        data.get("white_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    );
    b.set_kb_disabled(
        data.get("disabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    );
    b.set_kb_brightness_level(
        data.get("brightness_level")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(0),
    );
    b.set_kb_brightness_max(
        data.get("brightness_max")
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(4),
    );
    b.set_kb_rainbow(
        data.get("rainbow")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    );

    let color = data.get("color");
    let ch = |k: &str| {
        color
            .and_then(|c| c.get(k))
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .unwrap_or(0)
    };
    let (er, eg, eb) = (ch("r"), ch("g"), ch("b"));
    b.set_kb_ec_r(er);
    b.set_kb_ec_g(eg);
    b.set_kb_ec_b(eb);
    // EC 0-50 → 0-255，仅用于预览
    let to8 = |v: i32| -> u8 { ((v.clamp(0, 50) as u32 * 255) / 50) as u8 };
    b.set_kb_color(slint::Color::from_rgb_u8(to8(er), to8(eg), to8(eb)));

    let first = !PICKER_INIT.swap(true, Ordering::Relaxed);
    let resync = PICKER_RESYNC.swap(false, Ordering::Relaxed);
    if first || resync {
        b.set_pick_r(to8(er) as i32);
        b.set_pick_g(to8(eg) as i32);
        b.set_pick_b(to8(eb) as i32);
    }
}

fn apply_ec(ui: &AppWindow, data: &Value) {
    let b = ui.global::<Backend>();
    let mut cells = Vec::new();
    if let Some(values) = data.get("values").and_then(|v| v.as_array()) {
        for item in values {
            let addr = item
                .get("addr")
                .and_then(|v| v.as_str())
                .unwrap_or("----")
                .to_string();
            let value = item.get("value").and_then(|v| v.as_i64()).unwrap_or(0) as u8;
            let bits: String = format!("{:08b}", value)
                .chars()
                .enumerate()
                .flat_map(|(i, c)| {
                    if i == 4 {
                        vec![' ', c]
                    } else {
                        vec![c]
                    }
                })
                .collect();
            cells.push(EcCell {
                addr: format!("0x{addr}").into(),
                value: value as i32,
                hex: format!("{value:02X}").into(),
                bits: bits.into(),
            });
        }
    }
    b.set_ec_cells(ModelRc::from(Rc::new(VecModel::from(cells))));
    b.set_ec_read_time(format!("{} 读取", now_hm()).into());
}

// ---------------------------------------------------------------- 动作

fn do_set_mode(weak: Weak<AppWindow>, cli: Arc<Client>, idx: i32) {
    if let Some(ui) = weak.upgrade() {
        ui.global::<Backend>().set_busy(true);
    }
    let w2 = weak.clone();
    let c2 = cli.clone();
    run_cmd(
        weak,
        cli,
        "mode",
        params1("value", idx),
        move |ui, res| {
            ui.global::<Backend>().set_busy(false);
            match res {
                Ok(data) => {
                    apply_status(ui, &data);
                    let name = mode_name(idx);
                    toast(ui, format!("已切换到「{name}」档"), 1);
                    push_log(ui, format!("切换到 {idx} 档 - {name}"), 1);
                    refresh_aux(w2, c2);
                }
                Err(e) => {
                    toast(ui, e.clone(), 3);
                    push_log(ui, format!("模式切换失败：{e}"), 3);
                }
            }
        },
    );
}

fn do_gpu_write(weak: Weak<AppWindow>, cli: Arc<Client>, action: String) {
    if let Some(ui) = weak.upgrade() {
        ui.global::<Backend>().set_busy(true);
    }
    let w2 = weak.clone();
    let c2 = cli.clone();
    let act = action.clone();
    run_cmd(
        weak,
        cli,
        "gpu",
        params1("action", &action),
        move |ui, res| {
            ui.global::<Backend>().set_busy(false);
            match res {
                Ok(_) => {
                    ui.global::<Backend>().set_reboot_pending(true);
                    let label = if act == "dgpu" { "独显直连" } else { "混合输出" };
                    toast(ui, format!("已写入「{label}」，重启后生效"), 2);
                    push_log(ui, format!("写入 UEFI 变量：{label}（需重启生效）"), 2);
                    refresh_aux(w2, c2);
                }
                Err(e) => {
                    toast(ui, e.clone(), 3);
                    push_log(ui, format!("写入 UEFI 变量失败：{e}"), 3);
                }
            }
        },
    );
}

fn do_kb(weak: Weak<AppWindow>, cli: Arc<Client>, action: &'static str, value: Option<String>) {
    let mut params = params1("action", action);
    if let Some(v) = value {
        params.insert("value".to_string(), Value::String(v));
    }
    let w2 = weak.clone();
    let c2 = cli.clone();
    run_cmd(weak, cli, "kb", params, move |ui, res| match res {
        Ok(data) => {
            if action == "reset" {
                // 重置会改写 0x0769-0x076B，需要把新颜色回填到取色器，否则界面仍显示旧色
                PICKER_RESYNC.store(true, Ordering::Relaxed);
                let c = data.get("color");
                let ch = |k: &str| {
                    c.and_then(|v| v.get(k))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(-1)
                };
                push_log(
                    ui,
                    format!(
                        "键盘背光：已恢复固件默认色（EC R/G/B = {}/{}/{}，0-50 值域）",
                        ch("r"),
                        ch("g"),
                        ch("b")
                    ),
                    0,
                );
            } else {
                push_log(ui, format!("键盘背光：{action}"), 0);
            }
            refresh_aux(w2, c2);
        }
        Err(e) => {
            toast(ui, e.clone(), 3);
            push_log(ui, format!("键盘背光 {action} 失败：{e}"), 3);
        }
    });
}

fn do_ec_read(weak: Weak<AppWindow>, cli: Arc<Client>, base: i32, count: i32) {
    let addr = format!("0x{:04X}", base.clamp(0, 0xFFFF));
    run_cmd(
        weak,
        cli,
        "ec-read",
        params2("addr", addr, "count", count.clamp(1, 32)),
        move |ui, res| match res {
            Ok(data) => apply_ec(ui, &data),
            Err(e) => {
                toast(ui, e.clone(), 3);
                push_log(ui, format!("读取 EC 失败：{e}"), 3);
            }
        },
    );
}

/// 只刷新显卡与键盘（模式切换 / 写入后调用）
fn refresh_aux(weak: Weak<AppWindow>, cli: Arc<Client>) {
    let (wg, wc) = (weak.clone(), cli.clone());
    run_cmd(
        weak,
        cli,
        "gpu",
        params1("action", "status"),
        move |ui, res| {
            if let Ok(d) = res {
                apply_gpu(ui, &d);
            }
        },
    );
    run_cmd(wg, wc, "kb", params1("action", "status"), move |ui, res| {
        if let Ok(d) = res {
            apply_kb(ui, &d);
        }
    });
}

fn do_refresh(weak: Weak<AppWindow>, cli: Arc<Client>) {
    if let Some(ui) = weak.upgrade() {
        ui.global::<Backend>().set_busy(true);
    }
    let (w1, c1) = (weak.clone(), cli.clone());
    let (w2, c2) = (weak.clone(), cli.clone());
    let (w3, c3) = (weak.clone(), cli.clone());

    run_cmd(w1, c1, "status", Map::new(), move |ui, res| {
        ui.global::<Backend>().set_busy(false);
        let b = ui.global::<Backend>();
        match res {
            Ok(data) => {
                b.set_connected(true);
                apply_status(ui, &data);
            }
            Err(e) => {
                b.set_connected(false);
                push_log(ui, format!("守护进程不可用：{e}"), 3);
                toast(ui, "未连接到守护进程：请先在终端运行 sudo firebat-cc serve", 3);
            }
        }
    });

    run_cmd(w2, c2, "gpu", params1("action", "status"), move |ui, res| {
        if let Ok(d) = res {
            apply_gpu(ui, &d);
        }
    });

    run_cmd(w3, c3, "kb", params1("action", "status"), move |ui, res| {
        if let Ok(d) = res {
            apply_kb(ui, &d);
        }
    });
}

/// 每个模式档位对应的 EC 模式位（与后端一致）
fn mode_bits_of(index: i32) -> &'static str {
    match index {
        1 => "A0",
        2 => "20",
        3 => "30",
        4 => "A0",
        _ => "--",
    }
}

fn handle_event(ui: &AppWindow, ev: Value, cli: Arc<Client>) {
    match ev.get("event").and_then(|v| v.as_str()) {
        Some("hello") => {
            let ver = ev.get("version").and_then(|v| v.as_str()).unwrap_or("");
            let b = ui.global::<Backend>();
            b.set_daemon_version(ver.into());
            b.set_connected(true);
            b.set_listening(false);
            // hello 只在新连接建立时到达：守护进程可能重启过，监听状态要重新举起
            LISTEN_ON.store(false, Ordering::Relaxed);
            ensure_listen(ui.as_weak(), cli);
        }
        Some("mode-changed") => {
            let to = ev
                .get("to")
                .and_then(|t| t.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let idx = ev
                .get("to")
                .and_then(|t| t.get("index"))
                .and_then(|i| i.as_i64())
                .unwrap_or(0) as i32;
            let from = ev
                .get("from")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            {
                // 先就地更新，界面立刻响应；随后拉一次 status 校正 bits/detail
                let b = ui.global::<Backend>();
                b.set_mode_index(idx);
                b.set_mode_name(to.clone().into());
                b.set_mode_bits(mode_bits_of(idx).into());
            }
            push_log(ui, format!("性能模式键：{from} → {to}"), 2);
            toast(ui, format!("模式已切换为 {to}"), 1);
            refresh_status(ui.as_weak(), cli);
        }
        _ => {}
    }
}

/// 只刷新模式状态（事件到达后校正 bits/detail），不惊动显卡/背光
fn refresh_status(weak: Weak<AppWindow>, cli: Arc<Client>) {
    run_cmd(weak, cli, "status", Map::new(), |ui, res| {
        if let Ok(d) = res {
            apply_status(ui, &d);
        }
    });
}

static LISTEN_ON: AtomicBool = AtomicBool::new(false);

/// 让守护进程进入「监听模式」：由它轮询 EC 0x0751，按键后按
/// 办公(1) → 游戏(2) → 狂暴(3) 的顺序推进档位，并推 mode-changed 事件过来。
/// 幂等：已开启时直接返回。
fn ensure_listen(weak: Weak<AppWindow>, cli: Arc<Client>) {
    if LISTEN_ON.swap(true, Ordering::Relaxed) {
        return;
    }
    // 注意：服务端按 JSON bool 解析 enable，不能传字符串
    let mut params = Map::new();
    params.insert("enable".to_string(), Value::Bool(true));
    run_cmd(weak, cli, "listen", params, |ui, res| match res {
        Ok(_) => {
            ui.global::<Backend>().set_listening(true);
            push_log(ui, "已开启监听模式：性能模式键由守护进程接管", 1);
        }
        Err(e) => {
            LISTEN_ON.store(false, Ordering::Relaxed);
            ui.global::<Backend>().set_listening(false);
            push_log(ui, format!("开启监听模式失败：{e}"), 3);
        }
    });
}

// ---------------------------------------------------------------- 回调接线

fn wire_callbacks(ui: &AppWindow, cli: Arc<Client>) {
    let weak0 = ui.as_weak();

    {
        let weak = weak0.clone();
        let cli = cli.clone();
        ui.global::<Backend>().on_refresh(move || {
            do_refresh(weak.clone(), cli.clone());
        });
    }

    {
        let weak = weak0.clone();
        let cli = cli.clone();
        ui.global::<Backend>().on_request_mode(move |idx| {
            do_set_mode(weak.clone(), cli.clone(), idx);
        });
    }

    {
        let weak = weak0.clone();
        let cli = cli.clone();
        ui.global::<Backend>().on_gpu_write(move |action| {
            do_gpu_write(weak.clone(), cli.clone(), action.to_string());
        });
    }

    {
        let weak = weak0.clone();
        let cli = cli.clone();
        ui.global::<Backend>().on_set_kb_color(move |r, g, b| {
            let hex = format!(
                "{:02x}{:02x}{:02x}",
                r.clamp(0, 255),
                g.clamp(0, 255),
                b.clamp(0, 255)
            );
            do_kb(weak.clone(), cli.clone(), "color", Some(hex));
        });
    }

    // 亮度节流：EC 写入不宜过密
    {
        let weak = weak0.clone();
        let cli = cli.clone();
        ui.global::<Backend>().on_kb_brightness(move |pct| {
            static LAST: Mutex<Option<Instant>> = Mutex::new(None);
            let pct = pct.clamp(0, 100);
            let now = Instant::now();
            {
                let mut g = LAST.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(t) = *g {
                    if now.duration_since(t) < Duration::from_millis(70) {
                        return;
                    }
                }
                *g = Some(now);
            }
            do_kb(
                weak.clone(),
                cli.clone(),
                "brightness",
                Some(pct.to_string()),
            );
        });
    }

    {
        let weak = weak0.clone();
        let cli = cli.clone();
        ui.global::<Backend>().on_set_kb_rainbow(move |on| {
            do_kb(
                weak.clone(),
                cli.clone(),
                "rainbow",
                Some(if on { "on".to_string() } else { "off".to_string() }),
            );
        });
    }

    {
        let weak = weak0.clone();
        let cli = cli.clone();
        ui.global::<Backend>().on_kb_power(move |on| {
            do_kb(
                weak.clone(),
                cli.clone(),
                if on { "on" } else { "off" },
                None,
            );
        });
    }

    {
        let weak = weak0.clone();
        let cli = cli.clone();
        ui.global::<Backend>().on_kb_reset(move || {
            do_kb(weak.clone(), cli.clone(), "reset", None);
        });
    }

    {
        let weak = weak0.clone();
        let cli = cli.clone();
        ui.global::<Backend>().on_ec_step(move |delta| {
            let Some(ui) = weak.upgrade() else { return };
            let b = ui.global::<Backend>();
            let base = (b.get_ec_base() + delta).clamp(0, 0xFFFF);
            b.set_ec_base(base);
            b.set_ec_addr_text(format!("0x{base:04X}").into());
            do_ec_read(weak.clone(), cli.clone(), base, b.get_ec_count());
        });
    }

    {
        let weak = weak0.clone();
        let cli = cli.clone();
        ui.global::<Backend>().on_ec_preset(move |addr| {
            let Some(ui) = weak.upgrade() else { return };
            let b = ui.global::<Backend>();
            let base = addr.clamp(0, 0xFFFF);
            b.set_ec_base(base);
            b.set_ec_addr_text(format!("0x{base:04X}").into());
            do_ec_read(weak.clone(), cli.clone(), base, b.get_ec_count());
        });
    }

    {
        let weak = weak0.clone();
        let cli = cli.clone();
        ui.global::<Backend>().on_ec_read(move || {
            let Some(ui) = weak.upgrade() else { return };
            let b = ui.global::<Backend>();
            do_ec_read(
                weak.clone(),
                cli.clone(),
                b.get_ec_base(),
                b.get_ec_count(),
            );
        });
    }

    {
        let weak = weak0.clone();
        let cli = cli.clone();
        ui.global::<Backend>().on_confirm_accept(move || {
            let Some(ui) = weak.upgrade() else { return };
            let b = ui.global::<Backend>();
            let action = b.get_pending_action().to_string();
            b.set_confirm_visible(false);
            b.set_pending_action("".into());

            if let Some(rest) = action.strip_prefix("mode:") {
                if let Ok(idx) = rest.parse::<i32>() {
                    do_set_mode(weak.clone(), cli.clone(), idx);
                }
            } else if let Some(rest) = action.strip_prefix("gpu:") {
                do_gpu_write(weak.clone(), cli.clone(), rest.to_string());
            } else if action == "kb:reset" {
                do_kb(weak.clone(), cli.clone(), "reset", None);
            }
        });
    }

    {
        let weak = weak0.clone();
        ui.global::<Backend>().on_confirm_cancel(move || {
            if let Some(ui) = weak.upgrade() {
                let b = ui.global::<Backend>();
                b.set_confirm_visible(false);
                b.set_pending_action("".into());
            }
        });
    }

    {
        let weak = weak0.clone();
        ui.global::<Backend>().on_dismiss_toast(move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<Backend>().set_toast_visible(false);
            }
        });
    }
}

// ---------------------------------------------------------------- 外观（跟随系统）

/// 配置文件：$XDG_CONFIG_HOME/firebat-cc/gui.conf（缺省 ~/.config/…）
fn config_file() -> Option<std::path::PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("firebat-cc").join("gui.conf"))
}

fn theme_mode_from_text(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(v) = line.trim().strip_prefix("theme=") {
            let v = v.trim();
            if v == "auto" || v == "light" || v == "dark" {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn load_theme_mode() -> String {
    config_file()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| theme_mode_from_text(&t))
        .unwrap_or_else(|| "auto".to_string())
}

fn save_theme_mode(mode: &str) {
    let Some(p) = config_file() else { return };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = format!("# firebat-cc-gui 配置\n# theme: auto（跟随系统）| light | dark\ntheme={mode}\n");
    if let Err(e) = std::fs::write(&p, body) {
        eprintln!("写入 {} 失败: {e}", p.display());
    }
}

fn run_capture(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// GTK 的 settings.ini：`gtk-application-prefer-dark-theme=1`
fn gtk_prefer_dark(rel: &str) -> Option<bool> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join(".config"))
        })?;
    let text = std::fs::read_to_string(base.join(rel)).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("gtk-application-prefer-dark-theme") {
            let v = v.trim_start_matches(['=', ' ']).trim().to_ascii_lowercase();
            return match v.as_str() {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                _ => None,
            };
        }
    }
    None
}

/// KDE 的 kdeglobals：用 `[Colors:Window] BackgroundNormal` 的亮度判断
fn kde_dark() -> Option<bool> {
    let home = std::env::var("HOME").ok()?;
    let text = std::fs::read_to_string(std::path::Path::new(&home).join(".config/kdeglobals")).ok()?;
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == "[Colors:Window]";
            continue;
        }
        if in_section {
            if let Some(v) = line.strip_prefix("BackgroundNormal=") {
                let parts: Vec<f32> = v
                    .split(',')
                    .filter_map(|s| s.trim().parse::<f32>().ok())
                    .collect();
                if parts.len() >= 3 {
                    // BT.601 亮度
                    let lum = (0.299 * parts[0] + 0.587 * parts[1] + 0.114 * parts[2]) / 255.0;
                    return Some(lum < 0.5);
                }
            }
        }
    }
    None
}

/// 探测系统是否使用深色外观。None = 无法判断（此时沿用手动/默认值）。
fn detect_system_dark() -> Option<bool> {
    // 显式覆盖：便于测试、CI 与非常规桌面
    if let Ok(v) = std::env::var("FIREBAT_CC_THEME") {
        match v.trim().to_ascii_lowercase().as_str() {
            "dark" => return Some(true),
            "light" => return Some(false),
            _ => {}
        }
    }

    // 1) GNOME / 走 xdg-desktop-portal 的桌面：gsettings 最权威
    if let Some(out) = run_capture("gsettings", &["get", "org.gnome.desktop.interface", "color-scheme"]) {
        let v = out.trim().trim_matches(['\'', '"']).to_ascii_lowercase();
        if v == "prefer-dark" {
            return Some(true);
        }
        if v == "prefer-light" {
            return Some(false);
        }
        // "default" = 未表态，继续看 GTK 配置
    }

    // 2) GTK 配置（gtk4 优先）
    if let Some(dark) = gtk_prefer_dark("gtk-4.0/settings.ini") {
        return Some(dark);
    }
    if let Some(dark) = gtk_prefer_dark("gtk-3.0/settings.ini") {
        return Some(dark);
    }

    // 3) KDE
    kde_dark()
}

/// 按模式解析出实际深浅。自动模式下检测不到时保持深色（界面设计基准）。
fn resolve_dark(mode: &str, system: Option<bool>) -> bool {
    match mode {
        "dark" => true,
        "light" => false,
        _ => system.unwrap_or(true),
    }
}

/// 启动时套用一次外观，并把模式同步给界面
fn init_theme(ui: &AppWindow) -> String {
    let mode = load_theme_mode();
    ui.global::<Backend>().set_theme_mode(mode.clone().into());
    let dark = resolve_dark(&mode, if mode == "auto" { detect_system_dark() } else { None });
    ui.global::<Theme>().set_dark(dark);
    mode
}

// ---------------------------------------------------------------- main

/// 无界面自检：验证 socket 连通性与「重置背光」链路。
/// 用于真机排查（不需要图形环境）：`firebat-cc-gui --self-test [--socket PATH]`
fn self_test(socket: &std::path::Path) -> Result<(), String> {
    let (tx, _rx) = mpsc::channel();
    let cli = Client::new(socket.to_path_buf(), tx);
    println!("socket: {}", socket.display());

    let color = |v: &Value| -> (i64, i64, i64) {
        let c = v.get("color");
        let ch = |k: &str| {
            c.and_then(|c| c.get(k))
                .and_then(|v| v.as_i64())
                .unwrap_or(-1)
        };
        (ch("r"), ch("g"), ch("b"))
    };

    let before = cli.call("kb", params1("action", "status"))?;
    println!("重置前 kb status: {before}");
    let (r0, g0, b0) = color(&before);

    let reset = cli.call("kb", params1("action", "reset"))?;
    println!("kb reset 返回   : {reset}");
    let (rr, rg, rb) = color(&reset);

    let after = cli.call("kb", params1("action", "status"))?;
    println!("重置后 kb status: {after}");
    let (r1, g1, b1) = color(&after);

    if (rr, rg, rb) != (r1, g1, b1) {
        return Err(format!(
            "reset 返回值与回读不一致：{rr}/{rg}/{rb} != {r1}/{g1}/{b1}"
        ));
    }
    if (r0, g0, b0) == (r1, g1, b1) {
        println!("提示：重置前后颜色相同（{r1}/{g1}/{b1}），可能本来就是默认色");
    } else {
        println!("通过：颜色 {r0}/{g0}/{b0} → {r1}/{g1}/{b1}（EC 0-50 值域）");
    }
    Ok(())
}

fn main() -> Result<(), slint::PlatformError> {
    let path = client::resolve_socket_path();

    // 外观诊断：打印配置文件、系统探测结果与最终解析，便于排查"跟随系统不生效"
    if std::env::args().any(|a| a == "--print-theme") {
        let mode = load_theme_mode();
        let sys = detect_system_dark();
        println!("配置文件: {}", config_file().map(|p| p.display().to_string()).unwrap_or_else(|| "(无)".into()));
        println!("配置模式: {mode}");
        println!("系统探测: {}", match sys { Some(true) => "深色", Some(false) => "浅色", None => "无法判断" });
        println!("最终解析: {}", if resolve_dark(&mode, sys) { "深色" } else { "浅色" });
        std::process::exit(0);
    }

    if std::env::args().any(|a| a == "--self-test") {
        let code = match self_test(&path) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("自检失败：{e}");
                1
            }
        };
        std::process::exit(code);
    }

    let (tx, rx) = mpsc::channel::<Value>();
    let cli = Arc::new(Client::new(path.clone(), tx));

    let ui = AppWindow::new()?;

    {
        let b = ui.global::<Backend>();
        b.set_socket_path(path.display().to_string().into());
        b.set_log(ModelRc::from(Rc::new(VecModel::from(Vec::<LogEntry>::new()))));
        b.set_ec_cells(ModelRc::from(Rc::new(VecModel::from(Vec::<EcCell>::new()))));
        let base = b.get_ec_base();
        b.set_ec_addr_text(format!("0x{base:04X}").into());
    }

    wire_callbacks(&ui, cli.clone());

    // ---------------- 外观：默认跟随系统 ----------------
    let theme_mode = Arc::new(Mutex::new(init_theme(&ui)));

    {
        let weak = ui.as_weak();
        let tm = theme_mode.clone();
        ui.global::<Backend>().on_set_theme_mode(move |m| {
            let mode = m.to_string();
            let mode = if matches!(mode.as_str(), "auto" | "light" | "dark") {
                mode
            } else {
                "auto".to_string()
            };
            save_theme_mode(&mode);
            if let Ok(mut g) = tm.lock() {
                *g = mode.clone();
            }
            if let Some(ui) = weak.upgrade() {
                ui.global::<Backend>().set_theme_mode(mode.clone().into());
            }
            // 探测可能要 fork gsettings，放后台线程，避免卡住点击
            let w = weak.clone();
            let m2 = mode.clone();
            std::thread::spawn(move || {
                let dark = resolve_dark(&m2, if m2 == "auto" { detect_system_dark() } else { None });
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        ui.global::<Theme>().set_dark(dark);
                    }
                });
            });
        });
    }

    // 自动模式：定期复检系统外观，桌面切换深浅色时界面跟着变
    {
        let weak = ui.as_weak();
        let tm = theme_mode.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            if weak.upgrade().is_none() {
                break;
            }
            let auto = tm.lock().map(|m| m.as_str() == "auto").unwrap_or(false);
            if !auto {
                continue;
            }
            let Some(dark) = detect_system_dark() else {
                continue;
            };
            let w = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    if ui.global::<Theme>().get_dark() != dark {
                        ui.global::<Theme>().set_dark(dark);
                        push_log(
                            &ui,
                            format!("跟随系统外观：已切换为{}色", if dark { "深" } else { "浅" }),
                            1,
                        );
                    }
                }
            });
        });
    }

    push_log(&ui, format!("界面已启动，socket: {}", path.display()), 0);
    if !path.exists() {
        push_log(
            &ui,
            "未找到 socket：请先运行 sudo firebat-cc serve（需 modprobe ec_sys write_support=1）",
            2,
        );
    }

    // 事件转发（hello / mode-changed）——mode-changed 由守护进程在监听模式下推送
    {
        let weak = ui.as_weak();
        let cli2 = cli.clone();
        std::thread::spawn(move || {
            while let Ok(ev) = rx.recv() {
                let w = weak.clone();
                let c = cli2.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        handle_event(&ui, ev, c);
                    }
                });
            }
        });
    }

    // 轮询：捕获快捷键 / 命令行导致的外部模式变化
    {
        let weak = ui.as_weak();
        let cli = cli.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(4000));
            if weak.upgrade().is_none() {
                break;
            }
            let res = cli.call("status", Map::new());
            let ok = res.is_ok();
            let data = res.ok();
            let w = weak.clone();
            let c = cli.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = w.upgrade() else { return };
                let b = ui.global::<Backend>();
                let was = b.get_connected();
                if ok {
                    b.set_connected(true);
                    if let Some(d) = &data {
                        apply_status(&ui, d);
                    }
                    // 守护进程可能重启过，重连后要重新举起监听
                    ensure_listen(w.clone(), c.clone());
                    if !was {
                        push_log(&ui, "已连接守护进程", 1);
                        do_refresh(w.clone(), c);
                    }
                } else if was {
                    b.set_connected(false);
                    b.set_listening(false);
                    LISTEN_ON.store(false, Ordering::Relaxed);
                    push_log(&ui, "与守护进程的连接已断开", 3);
                }
            });
        });
    }

    ui.global::<Backend>().invoke_refresh();
    let r = ui.run();

    // 退出前主动示意取消监听：正常情况下守护进程会随连接断开自动摘除，
    // 这里再显式关一次，避免守护进程保留了空 owner（老版本守护进程也兼容）。
    let mut params = Map::new();
    params.insert("enable".to_string(), Value::Bool(false));
    let _ = cli.call("listen", params);
    r
}
