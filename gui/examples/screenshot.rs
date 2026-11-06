//! 无显示器环境下把界面渲染成图片，用于文档截图与视觉回归检查。
//!
//!   cargo run --release --example screenshot -- /tmp/firebat
//!
//! 输出 `/tmp/firebat-0.ppm` … `/tmp/firebat-4.ppm`（每页一张，P6 格式）。

slint::include_modules!();

use rgb::RGB8;
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, PointerEventButton, WindowAdapter, WindowEvent};
use std::io::Write;
use std::rc::Rc;

const W: u32 = 1200;
const H: u32 = 830;

struct Headless {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

fn save_ppm(path: &str, buf: &[RGB8]) {
    let mut f = std::fs::File::create(path).expect("创建图片失败");
    write!(f, "P6\n{} {}\n255\n", W, H).unwrap();
    for p in buf {
        f.write_all(&[p.r, p.g, p.b]).unwrap();
    }
}

fn main() {
    let prefix = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/firebat".to_string());

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Headless {
        window: window.clone(),
    }))
    .expect("设置平台失败");

    let ui = AppWindow::new().expect("构建界面失败");

    // ---------------- 演示数据（模拟已连接守护进程） ----------------
    {
        let b = ui.global::<Backend>();
        b.set_connected(true);
        b.set_listening(true); // 概览页「模式键 / 监听中」
        b.set_daemon_version("0.6.0".into());
        b.set_socket_path("/run/firebat-cc.sock".into());
        b.set_mode_index(3);
        b.set_mode_name("狂暴".into());
        b.set_mode_bits("30".into());
        b.set_mode_raw(0x30);
        b.set_backend_name("ec-direct".into());
        b.set_status_detail("EC 0x0751 = 0x30（BIT5 | BIT4 = 狂暴），风扇增强 BIT6 = 0".into());

        b.set_gpu_state("hybrid".into());
        b.set_gpu_raw(0);
        b.set_gpu_var("UniWillVariable".into());
        b.set_gpu_payload_len(188);

        b.set_kb_white_only(false);
        b.set_kb_disabled(false);
        b.set_kb_brightness_level(3);
        b.set_kb_brightness_max(4);
        b.set_kb_rainbow(false);
        b.set_kb_ec_r(50);
        b.set_kb_ec_g(28);
        b.set_kb_ec_b(12);
        b.set_kb_color(slint::Color::from_rgb_u8(255, 142, 61));
        b.set_pick_r(255);
        b.set_pick_g(142);
        b.set_pick_b(61);

        b.set_ec_base(0x751);
        b.set_ec_addr_text("0x0751".into());
        b.set_ec_count(8);
        b.set_ec_read_time("12:31 读取".into());

        let cells = (0..8u16)
            .map(|i| {
                let value = [0x30u8, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00][i as usize % 8];
                let bits: String = format!("{value:08b}")
                    .chars()
                    .enumerate()
                    .flat_map(|(k, c)| if k == 4 { vec![' ', c] } else { vec![c] })
                    .collect();
                EcCell {
                    addr: format!("0x{:04X}", 0x751 + i).into(),
                    value: value as i32,
                    hex: format!("{value:02X}").into(),
                    bits: bits.into(),
                }
            })
            .collect::<Vec<_>>();
        b.set_ec_cells(slint::ModelRc::from(Rc::new(slint::VecModel::from(cells))));

        let logs = vec![
            LogEntry {
                time: "12:31".into(),
                text: "切换到 3 档 - 狂暴".into(),
                kind: 1,
            },
            LogEntry {
                time: "12:30".into(),
                text: "键盘背光：color".into(),
                kind: 0,
            },
            LogEntry {
                time: "12:28".into(),
                text: "已连接守护进程".into(),
                kind: 1,
            },
            LogEntry {
                time: "12:28".into(),
                text: "界面已启动，socket: /run/firebat-cc.sock".into(),
                kind: 0,
            },
        ];
        b.set_log(slint::ModelRc::from(Rc::new(slint::VecModel::from(logs))));
    }

    window.set_size(slint::PhysicalSize::new(W, H));
    ui.show().expect("窗口显示失败");

    let mut buf = vec![RGB8::new(0, 0, 0); (W * H) as usize];

    let mut shoot = |ui: &AppWindow, buf: &mut Vec<RGB8>, name: &str| {
        ui.window().request_redraw();
        for _ in 0..6 {
            slint::platform::update_timers_and_animations();
            window.draw_if_needed(|renderer| {
                renderer.render(buf, W as usize);
            });
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let path = format!("{prefix}-{name}.ppm");
        save_ppm(&path, buf);
        println!("已渲染 {path}");
    };

    for page in 0..5 {
        ui.global::<Nav>().set_ec_debug_enabled(page == 4);
        ui.global::<Nav>().set_page(page);
        shoot(&ui, &mut buf, &page.to_string());
    }

    // 浅色主题
    ui.global::<Theme>().set_dark(false);
    ui.global::<Nav>().set_page(3);
    shoot(&ui, &mut buf, "light");
    ui.global::<Theme>().set_dark(true);

    // 交互回归：滑杆只能「按下后拖动」改值，纯悬停不得改变
    {
        ui.global::<Nav>().set_page(3);
        slint::platform::update_timers_and_animations();
        window.draw_if_needed(|_| {});

        let b = ui.global::<Backend>();
        let before = b.get_pick_r();

        // 悬停横扫 R 通道滑杆所在行（颜色卡内第一条滑杆，y≈297）
        for x in (300..700).step_by(20) {
            let _ = ui.window().dispatch_event(WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(x as f32, 297.0),
            });
            slint::platform::update_timers_and_animations();
        }
        let after_hover = b.get_pick_r();

        // 同一点按下：应当立即改值（证明坐标确实落在滑杆上）
        let _ = ui.window().dispatch_event(WindowEvent::PointerPressed {
            position: slint::LogicalPosition::new(320.0, 297.0),
            button: PointerEventButton::Left,
        });
        slint::platform::update_timers_and_animations();
        let after_press = b.get_pick_r();

        // 按住拖动：应当继续跟随
        for x in [360.0f32, 420.0, 480.0, 560.0] {
            let _ = ui.window().dispatch_event(WindowEvent::PointerMoved {
                position: slint::LogicalPosition::new(x, 297.0),
            });
            slint::platform::update_timers_and_animations();
        }
        let after_drag = b.get_pick_r();

        let _ = ui.window().dispatch_event(WindowEvent::PointerReleased {
            position: slint::LogicalPosition::new(560.0, 297.0),
            button: PointerEventButton::Left,
        });
        slint::platform::update_timers_and_animations();

        let hover_ok = after_hover == before;
        let press_ok = after_press != before;
        let drag_ok = after_drag > after_press;
        println!(
            "滑杆交互回归：悬停 {} → {}（{}），按下 → {}（{}），拖动 → {}（{}）",
            before,
            after_hover,
            if hover_ok { "通过：未改变" } else { "失败：悬停改变了数值" },
            after_press,
            if press_ok { "通过：已改变" } else { "失败：按下未命中滑杆" },
            after_drag,
            if drag_ok { "通过：跟随拖动" } else { "失败：拖动未生效" },
        );
        b.set_pick_r(before);
    }

    // 二次确认对话框
    {
        let b = ui.global::<Backend>();
        b.set_confirm_title("切换到独显直连？".into());
        b.set_confirm_body("将修改 UEFI 厂商变量（GUID 9f33f85c…c593）中的 OemDisplayMode 字段并整块写回。此类写入属于高风险操作，写入后需要重启才会生效，重启前请勿断电。".into());
        b.set_confirm_ok("写入并重启后生效".into());
        b.set_confirm_danger(true);
        b.set_confirm_visible(true);
        b.set_pending_action("gpu:dgpu".into());
    }
    ui.global::<Nav>().set_page(2);
    shoot(&ui, &mut buf, "confirm");
}
