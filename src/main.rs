//! firebat-cc v0.2.0 — Firebat GX（火影）性能模式与显卡输出控制工具（Linux）
//!
//! 依据对 Windows 端 ControlCenter 5.49.51.44（Firebat 定制版）的逆向结果实现。
//! 功能：
//!   - 四档性能模式切换（1 办公 / 2 游戏(均衡) / 3 狂暴 / 4 自定义）
//!     自定义模式（4 档）暂等同于 1 档（办公），接口保留，后续版本扩展。
//!   - 显卡输出切换（混合输出 hybrid / 独显直连 dgpu，写入 UEFI 变量，重启生效）
//!
//! 性能模式原理（逆向报告 §5.1 / 附录 A.1 与内核 uniwill 驱动交叉验证）：
//!   - 模式寄存器 EC_ADDR_MANUAL_FAN_CTRL = 0x0751（即报告中的 ADDR_MAFAN_CONTROL_BYTE = 1873）
//!   - 模式值是位组合：
//!       办公   office   = BIT(7) | BIT(5) = 0xA0
//!       均衡   balance  = BIT(5)          = 0x20
//!       狂暴   turbo    = BIT(5) | BIT(4) = 0x30
//!     （BIT(6) 为风扇增强位，读写时予以保留）
//!   - EC 扩展地址（>0xFF）通过标准 EC 256 字节空间中的间接寻址协议访问：
//!       0x8A LDAT(地址低) 0x8B HDAT(地址高) 0x8C FLAGS 0x8D CMDL(数据低) 0x8E CMDH(数据高)
//!     协议流程与 tuxedo-drivers uniwill_wmi.c 的 direct 模式一致。
//!   - 用户态访问路径：
//!       ec-direct        /sys/kernel/debug/ec/ec0/io（需 root 且加载 ec_sys write_support=1）
//!       platform-profile /sys/class/platform-profile/（较新内核的 uniwill-laptop 驱动）
//!
//! 显卡输出切换原理（对安装包内 UEFI_Firmware.dll / GCUService.exe / GCUBridge.exe /
//! DefaultTool.exe 的静态逆向）：
//!   - MUX 状态保存在 UEFI 厂商变量（GUID {9f33f85c-13ca-4fd1-9c4a-96217722c593}）：
//!       UniWillVariable（首选）/ OemMagicVariable（回退），BIOS 在下次开机时应用。
//!   - 变量内容即 NVRAM_STRUCT（188 字节，.NET 默认 marshal 布局），
//!       OemDisplayMode 字段位于偏移 0x62(98)（--mux-offset 可覆盖）。
//!   - Windows 端流程：GetFwVars 读整块 → 改一个字段 → WriteUefi 整块写回；
//!   本工具在 Linux 上经 efivarfs（/sys/firmware/efi/efivars/）做同样的读-改-写。
//!   - ⚠️ 0/1 与混合/独显的对应关系基于工厂默认值推断（0=混合输出），提供
//!       --hybrid-value/--dgpu-value 覆盖；真机确认方法见 uefi-dump 命令说明。
//!
//! 单文件设计：编译产物为单个独立二进制，无需安装，复制即可用。
//!
//! ⚠️ 直接写 EC 属于底层硬件操作。请在了解风险的前提下在真机上测试；
//!    本工具对模式寄存器只做「读-改-写（仅触碰模式位）+ 回读校验」，不改动其他位。

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::raw::{c_int, c_ulong};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// efivarfs 文件默认带 immutable 标志（写前需清除，写后恢复）
extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn flock(fd: c_int, operation: c_int) -> c_int;
    fn signal(sig: c_int, handler: usize) -> usize;
}
const FS_IOC_GETFLAGS: c_ulong = 0x8008_6601;
const FS_IOC_SETFLAGS: c_ulong = 0x4008_6602;
const FS_IMMUTABLE_FL: i32 = 0x10;

// ---------------------------------------------------------------- EC 常量

/// 模式寄存器：ADDR_MAFAN_CONTROL_BYTE（逆向报告）= EC_ADDR_MANUAL_FAN_CTRL（内核命名）
const MODE_REG_DEFAULT: u16 = 0x0751;

/// 模式位掩码：FAN_MODE_TURBO BIT(4) | FAN_MODE_HIGH BIT(5) | FAN_MODE_USER BIT(7)
const PERF_MODE_MASK: u8 = 0b1011_0000; // 0xB0
/// 办公 / low-power
const PERF_MODE_OFFICE: u8 = 0b1010_0000; // 0xA0 = BIT7|BIT5
/// 均衡(游戏) / balanced
const PERF_MODE_BALANCE: u8 = 0b0010_0000; // 0x20 = BIT5
/// 狂暴 / performance
const PERF_MODE_TURBO: u8 = 0b0011_0000; // 0x30 = BIT5|BIT4
/// 风扇增强位（FAN_MODE_BOOST），切换模式时保留
const FAN_MODE_BOOST: u8 = 0b0100_0000; // BIT6

/// 间接寻址协议寄存器（标准 EC 空间）
const REG_LDAT: u8 = 0x8A; // 地址低字节
const REG_HDAT: u8 = 0x8B; // 地址高字节
const REG_FLAGS: u8 = 0x8C; // 标志寄存器
const REG_CMDL: u8 = 0x8D; // 数据低字节
const REG_CMDH: u8 = 0x8E; // 数据高字节

const FLAG_RFLG: u8 = 1 << 0; // 请求读
const FLAG_WFLG: u8 = 1 << 1; // 请求写
const FLAG_BFLG: u8 = 1 << 2; // 操作占用标志
const FLAG_DRDY: u8 = 1 << 7; // 数据就绪

const EC_SYS_PATH: &str = "/sys/kernel/debug/ec/ec0/io";
const BUSY_WAIT_CYCLES: u32 = 30;
const BUSY_WAIT_DELAY_MS: u64 = 15;
const RETRY_TIMES: usize = 3;

// 键盘背光寄存器（Uniwill ITE EC；与 tuxedo-drivers uniwill_interfaces.h 交叉验证一致）
const KBD_BL_STATUS: u16 = 0x078C; // bit0=白色单色键盘；bit1=背光禁用(1=灭)；bit5-7=亮度级(0-4)；写 0x10=应用/重置
const KBD_BL_STATUS_WHITE_ONLY: u8 = 0x01;
const KBD_BL_STATUS_DISABLED: u8 = 0x02; // 真机验证：bit1 置位 = 背光关闭（tuxedo uniwill_write_kbd_bl_enable 反逻辑）
const KBD_BL_STATUS_RESET: u8 = 0x10; // 一次性应用位（EC 消费后自动清零，真机验证）；tuxedo 命名 SUBCMD_RESET
const KBD_BL_STATUS_BRI_MASK: u8 = 0xE0; // bit5-7 = 亮度级
const KBD_BL_STATUS_BRI_MAX: u8 = 4; // 1-zone RGB 最大亮度级（tuxedo UNIWILL_KBD_BRIGHTNESS_MAX_1_ZONE_RGB）
const KBD_BL_RGB_MODE: u16 = 0x0767; // bit5 = APPLY_COLOR 应用颜色；bit7 = RAINBOW 彩虹循环
const KBD_BL_RGB_MODE_APPLY: u8 = 0x20;
const KBD_BL_RGB_MODE_RAINBOW: u8 = 0x80;
const KBD_BL_RGB_RED: u16 = 0x0769; // = 报告附录 A 的 1897/1898/1899；值域 0-50（tuxedo tf_convert_rgb_range）
const KBD_BL_RGB_GREEN: u16 = 0x076A;
const KBD_BL_RGB_BLUE: u16 = 0x076B;
const KBD_BL_DEFAULT_RED: u16 = 0x076C; // Windows 端「默认色」R/G/B（报告 1900/1901/1902）
const KBD_BL_DEFAULT_GREEN: u16 = 0x076D;
const KBD_BL_DEFAULT_BLUE: u16 = 0x076E;
const KBD_BL_WHITE_BRI_IMM: u16 = 0x1802; // 白色单色键盘立即生效亮度 0-255（推断）
/// 颜色通道 0-255 → EC 值域 0-50（tuxedo tf_convert_rgb_range 等价实现）
fn kb_convert_channel(v: u8) -> u8 {
    ((v as u32) * 200 / (255 * 4)) as u8
}
const FEATURES_1: u16 = 0x0766; // bit2 = 支持单区 RGB 键盘（tuxedo FEATURES_1_BIT_1_ZONE_RGB_KB）
const FEATURES_1_BIT_1ZONE_RGB: u8 = 0x04;

// ---------------------------------------------------------------- 模式定义

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Office, // 1 办公
    Game,   // 2 游戏/均衡
    Turbo,  // 3 狂暴
    Custom, // 4 自定义（v0.1 等同于 Office，接口保留）
}

impl Mode {
    fn parse(s: &str) -> Option<Mode> {
        match s.to_ascii_lowercase().as_str() {
            "1" | "office" | "办公" => Some(Mode::Office),
            "2" | "game" | "balance" | "游戏" | "均衡" => Some(Mode::Game),
            "3" | "turbo" | "狂暴" => Some(Mode::Turbo),
            "4" | "custom" | "自定义" => Some(Mode::Custom),
            _ => None,
        }
    }

    fn index(&self) -> u8 {
        match self {
            Mode::Office => 1,
            Mode::Game => 2,
            Mode::Turbo => 3,
            Mode::Custom => 4,
        }
    }

    fn zh(&self) -> &'static str {
        match self {
            Mode::Office => "办公",
            Mode::Game => "游戏(均衡)",
            Mode::Turbo => "狂暴",
            Mode::Custom => "自定义",
        }
    }

    /// 写入 EC 模式寄存器的位组合。自定义模式 v0.1 等同于办公档。
    fn ec_bits(&self) -> u8 {
        match self {
            Mode::Office | Mode::Custom => PERF_MODE_OFFICE,
            Mode::Game => PERF_MODE_BALANCE,
            Mode::Turbo => PERF_MODE_TURBO,
        }
    }
}

/// 从模式寄存器当前值推断模式（EC 不保存 Custom 标记，显示为 Office）
fn mode_from_bits(bits: u8) -> Option<Mode> {
    match bits & PERF_MODE_MASK {
        PERF_MODE_TURBO => Some(Mode::Turbo),
        PERF_MODE_BALANCE => Some(Mode::Game),
        PERF_MODE_OFFICE => Some(Mode::Office),
        _ => None,
    }
}

// ---------------------------------------------------------------- 后端：ec-direct

struct EcBackend {
    io: File,
    mode_reg: u16,
}

impl EcBackend {
    fn open(mode_reg: u16) -> Result<EcBackend, String> {
        let io = OpenOptions::new()
            .read(true)
            .write(true)
            .open(EC_SYS_PATH)
            .map_err(|e| {
                format!(
                    "无法打开 {EC_SYS_PATH}: {e}\n提示:\n  1) 需要以 root 运行: sudo firebat-cc ...\n  2) 需要加载 ec_sys 并开启写支持: sudo modprobe ec_sys write_support=1\n  （或改用新内核 uniwill 驱动的 platform-profile 后端: --backend platform-profile）"
                )
            })?;
        Ok(EcBackend { io, mode_reg })
    }

    /// 直接读写标准 EC 空间（0x00–0xFF）的单个字节
    fn raw_read(&mut self, reg: u8) -> Result<u8, String> {
        self.io
            .seek(SeekFrom::Start(reg as u64))
            .map_err(|e| format!("seek 0x{reg:02X} 失败: {e}"))?;
        let mut buf = [0u8; 1];
        self.io
            .read_exact(&mut buf)
            .map_err(|e| format!("读取 EC 0x{reg:02X} 失败: {e}"))?;
        Ok(buf[0])
    }

    fn raw_write(&mut self, reg: u8, val: u8) -> Result<(), String> {
        self.io
            .seek(SeekFrom::Start(reg as u64))
            .map_err(|e| format!("seek 0x{reg:02X} 失败: {e}"))?;
        self.io.write_all(&[val]).map_err(|e| {
            format!(
                "写入 EC 0x{reg:02X} 失败: {e}\n（若为 Permission denied，请先执行: sudo modprobe ec_sys write_support=1）"
            )
        })?;
        Ok(())
    }

    /// 等待 DRDY（数据就绪）标志
    fn wait_drdy(&mut self) -> Result<(), String> {
        for _ in 0..BUSY_WAIT_CYCLES {
            std::thread::sleep(Duration::from_millis(BUSY_WAIT_DELAY_MS));
            let flags = self.raw_read(REG_FLAGS)?;
            if flags & FLAG_DRDY != 0 {
                return Ok(());
            }
        }
        Err("EC 操作超时（等待 DRDY 失败）".into())
    }

    /// Uniwill 间接寻址协议读（对应 uw_ec_read_addr_direct）
    fn uw_read(&mut self, addr: u16) -> Result<u8, String> {
        let mut last_err = String::new();
        for _ in 0..RETRY_TIMES {
            match self.uw_read_once(addr) {
                Ok(v) => return Ok(v),
                Err(e) => last_err = e,
            }
        }
        Err(format!(
            "读取 EC 0x{addr:04X} 失败（已重试 {RETRY_TIMES} 次）: {last_err}"
        ))
    }

    fn uw_read_once(&mut self, addr: u16) -> Result<u8, String> {
        let mut flags = self.raw_read(REG_FLAGS)?;
        flags |= FLAG_BFLG;
        self.raw_write(REG_FLAGS, flags)?;

        self.raw_write(REG_LDAT, (addr & 0xff) as u8)?;
        self.raw_write(REG_HDAT, (addr >> 8) as u8)?;

        flags &= !FLAG_DRDY;
        flags |= FLAG_RFLG;
        self.raw_write(REG_FLAGS, flags)?;

        let res = self.wait_drdy().and_then(|_| self.raw_read(REG_CMDL));
        let _ = self.raw_write(REG_FLAGS, 0x00); // 无论成败都清标志
        res
    }

    /// Uniwill 间接寻址协议写（对应 uw_ec_write_addr_direct）
    fn uw_write(&mut self, addr: u16, data: u8) -> Result<(), String> {
        let mut last_err = String::new();
        for _ in 0..RETRY_TIMES {
            match self.uw_write_once(addr, data) {
                Ok(()) => return Ok(()),
                Err(e) => last_err = e,
            }
        }
        Err(format!(
            "写入 EC 0x{addr:04X} 失败（已重试 {RETRY_TIMES} 次）: {last_err}"
        ))
    }

    fn uw_write_once(&mut self, addr: u16, data: u8) -> Result<(), String> {
        let mut flags = self.raw_read(REG_FLAGS)?;
        flags |= FLAG_BFLG;
        self.raw_write(REG_FLAGS, flags)?;

        self.raw_write(REG_LDAT, (addr & 0xff) as u8)?;
        self.raw_write(REG_HDAT, (addr >> 8) as u8)?;
        self.raw_write(REG_CMDL, data)?;
        self.raw_write(REG_CMDH, 0x00)?;

        flags &= !FLAG_DRDY;
        flags |= FLAG_WFLG;
        self.raw_write(REG_FLAGS, flags)?;

        let res = self.wait_drdy();
        let _ = self.raw_write(REG_FLAGS, 0x00);
        res
    }

    fn read_state(&mut self) -> Result<(Option<Mode>, String), String> {
        let bits = self.uw_read(self.mode_reg)?;
        let boost = if bits & FAN_MODE_BOOST != 0 {
            " [风扇增强 BIT6=1]"
        } else {
            ""
        };
        let detail = format!(
            "EC 0x{:04X} = 0x{:02X} (0b{:08b}){}",
            self.mode_reg, bits, bits, boost
        );
        Ok((mode_from_bits(bits), detail))
    }

    /// 读-改-写：仅触碰模式位，保留 BIT6（风扇增强）等其他位；写后回读校验。
    fn write_mode(&mut self, m: Mode) -> Result<(), String> {
        let old = self.uw_read(self.mode_reg)?;
        let new = (old & !PERF_MODE_MASK) | m.ec_bits();
        if new == old {
            return Ok(()); // 已处于目标模式
        }
        self.uw_write(self.mode_reg, new)?;
        let back = self.uw_read(self.mode_reg)?;
        if back & PERF_MODE_MASK != new & PERF_MODE_MASK {
            return Err(format!(
                "回读校验失败: 期望 0x{:02X}，实际 0x{:02X}",
                new & PERF_MODE_MASK,
                back & PERF_MODE_MASK
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- 后端：platform-profile

struct PpBackend {
    dir: PathBuf,
}

/// 在 /sys/class/platform-profile/ 下找支持 low-power/balanced/performance 的设备，
/// 优先选 uniwill 驱动提供的。
fn find_pp_dir() -> Option<PathBuf> {
    let base = Path::new("/sys/class/platform-profile");
    let entries = std::fs::read_dir(base).ok()?;
    let mut fallback: Option<PathBuf> = None;
    for e in entries.flatten() {
        let choices = std::fs::read_to_string(e.path().join("choices")).unwrap_or_default();
        if !(choices.contains("performance") && choices.contains("low-power")) {
            continue;
        }
        let name = std::fs::read_to_string(e.path().join("name")).unwrap_or_default();
        if name.to_ascii_lowercase().contains("uniwill") {
            return Some(e.path());
        }
        if fallback.is_none() {
            fallback = Some(e.path());
        }
    }
    fallback
}

impl PpBackend {
    fn read_state(&self) -> Result<(Option<Mode>, String), String> {
        let cur = std::fs::read_to_string(self.dir.join("profile"))
            .map_err(|e| {
                format!(
                    "读取 {} 失败: {e}",
                    self.dir.join("profile").display()
                )
            })?
            .trim()
            .to_string();
        let mode = match cur.as_str() {
            "low-power" => Some(Mode::Office),
            "balanced" => Some(Mode::Game),
            "performance" => Some(Mode::Turbo),
            _ => None,
        };
        Ok((
            mode,
            format!("platform profile = {cur} ({})", self.dir.display()),
        ))
    }

    fn write_mode(&self, m: Mode) -> Result<(), String> {
        let target = match m {
            Mode::Office | Mode::Custom => "low-power",
            Mode::Game => "balanced",
            Mode::Turbo => "performance",
        };
        std::fs::write(self.dir.join("profile"), target).map_err(|e| {
            format!(
                "写入 {} 失败: {e}（需要 root 权限）",
                self.dir.join("profile").display()
            )
        })
    }
}

// ---------------------------------------------------------------- 后端：uefi-var（显卡输出 MUX）

/// UEFI 厂商变量 GUID（UEFI_Firmware.dll 内硬编码）
const UEFI_VAR_GUID: &str = "9f33f85c-13ca-4fd1-9c4a-96217722c593";
/// 变量名：与 Windows 端 ReadUefi 的顺序一致（先 OemMagicVariable，回退 UniWillVariable）
const UEFI_VAR_NAMES: [&str; 2] = ["OemMagicVariable", "UniWillVariable"];
/// NVRAM_STRUCT 中 OemDisplayMode 的默认偏移（.NET 默认 marshal 布局）
const MUX_OFFSET_DEFAULT: u16 = 98;
/// 默认值映射（工厂默认推断：0=混合输出，1=独显直连）
const MUX_HYBRID_DEFAULT: u8 = 0;
const MUX_DGPU_DEFAULT: u8 = 1;
/// NVRAM_STRUCT 已知字段（偏移按 .NET 默认 marshal 布局计算）
const NVRAM_FIELDS: [(u16, u8, &str); 8] = [
    (0, 2, "OemBoardSsid(u16)"),
    (2, 1, "SupportByte"),
    (3, 1, "ProjectID"),
    (4, 1, "KeyboardType"),
    (44, 1, "PowerMode"),
    (48, 1, "MemoryOverClockSwitch"),
    (97, 1, "ApUseFlag"),
    (98, 1, "OemDisplayMode(MUX)"),
];

struct UefiBackend {
    path: PathBuf,
    var_name: &'static str,
    mux_offset: u16,
    hybrid_value: u8,
    dgpu_value: u8,
}

impl UefiBackend {
    fn open(mux_offset: u16, hybrid_value: u8, dgpu_value: u8) -> Result<UefiBackend, String> {
        let base = Path::new("/sys/firmware/efi/efivars");
        if !base.exists() {
            return Err(
                "未找到 efivarfs（/sys/firmware/efi/efivars）：本机可能以 Legacy BIOS 模式启动，或未挂载 debugfs。\n提示: sudo mount -t efivarfs efivarfs /sys/firmware/efi/efivars".into(),
            );
        }
        for name in UEFI_VAR_NAMES {
            let p = base.join(format!("{name}-{UEFI_VAR_GUID}"));
            if p.exists() {
                return Ok(UefiBackend {
                    path: p,
                    var_name: name,
                    mux_offset,
                    hybrid_value,
                    dgpu_value,
                });
            }
        }
        Err(format!(
            "efivarfs 中未找到 Uniwill 厂商变量（{}/OemMagicVariable-GUID）。\n此机型可能未暴露该接口。",
            UEFI_VAR_NAMES[0]
        ))
    }

    /// 读取变量：返回 (attributes, payload)
    fn read(&self) -> Result<(u32, Vec<u8>), String> {
        let raw = std::fs::read(&self.path)
            .map_err(|e| format!("读取 {} 失败: {e}（需要 root）", self.path.display()))?;
        if raw.len() < 5 {
            return Err(format!("{} 内容异常（{} 字节）", self.path.display(), raw.len()));
        }
        let attrs = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        Ok((attrs, raw[4..].to_vec()))
    }

    /// 清除/恢复 efivarfs 文件的 immutable 标志（内核默认对 efivarfs 文件置位：
    /// 带标志时连写模式打开都会 EPERM，必须先用只读 fd 清除）
    fn set_immutable(&self, enable: bool) -> Result<(), String> {
        let file = File::open(&self.path)
            .map_err(|e| format!("打开 {} 失败: {e}（需要 root）", self.path.display()))?;
        let fd = file.as_raw_fd();
        let mut flags: i32 = 0;
        unsafe {
            if ioctl(fd, FS_IOC_GETFLAGS, &mut flags as *mut i32) != 0 {
                return Ok(()); // 不支持 ioctl 则跳过（旧内核无 immutable 行为）
            }
            if enable {
                if flags & FS_IMMUTABLE_FL == 0 {
                    let v = flags | FS_IMMUTABLE_FL;
                    let _ = ioctl(fd, FS_IOC_SETFLAGS, &v as *const i32);
                }
            } else if flags & FS_IMMUTABLE_FL != 0 {
                let v = flags & !FS_IMMUTABLE_FL;
                if ioctl(fd, FS_IOC_SETFLAGS, &v as *const i32) != 0 {
                    return Err(
                        "无法清除 efivarfs 文件的 immutable 标志（CAP_LINUX_IMMUTABLE 要求真正 root；\
                         容器/沙箱内的 root 无效。也可手动: sudo chattr -i <变量文件>）"
                            .into(),
                    );
                }
            }
        }
        Ok(())
    }

    /// 整块写回（先清 immutable → 写 → 回读校验 → 恢复 immutable）
    fn write(&self, attrs: u32, payload: &[u8]) -> Result<(), String> {
        self.set_immutable(false)?;
        let result = self.write_unlocked(attrs, payload);
        let _ = self.set_immutable(true);
        result
    }

    fn write_unlocked(&self, attrs: u32, payload: &[u8]) -> Result<(), String> {
        // 注意：efivarfs 不支持 lseek（Illegal seek），写后校验通过重新打开文件读取
        let mut file = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .map_err(|e| format!("打开 {} 失败: {e}（需要 root）", self.path.display()))?;

        let mut buf = Vec::with_capacity(4 + payload.len());
        buf.extend_from_slice(&attrs.to_le_bytes());
        buf.extend_from_slice(payload);

        file.write_all(&buf)
            .map_err(|e| format!("写入 {} 失败: {e}", self.path.display()))?;
        file.sync_all().ok();
        drop(file);

        // 回读校验
        let back = std::fs::read(&self.path)
            .map_err(|e| format!("回读失败: {e}"))?;
        if back != buf {
            return Err("回读校验失败：写入后的变量内容与预期不一致".into());
        }
        Ok(())
    }

    fn read_mux(&self) -> Result<u8, String> {
        let (_, payload) = self.read()?;
        if (self.mux_offset as usize) >= payload.len() {
            return Err(format!(
                "变量负载仅 {} 字节，小于 MUX 偏移 {}",
                payload.len(),
                self.mux_offset
            ));
        }
        Ok(payload[self.mux_offset as usize])
    }

    fn write_mux(&self, val: u8) -> Result<(), String> {
        let (attrs, mut payload) = self.read()?;
        if (self.mux_offset as usize) >= payload.len() {
            return Err(format!(
                "变量负载仅 {} 字节，小于 MUX 偏移 {}",
                payload.len(),
                self.mux_offset
            ));
        }
        payload[self.mux_offset as usize] = val;
        self.write(attrs, &payload)
    }
}

// ---------------------------------------------------------------- 后端分发

enum Tool {
    Ec(EcBackend),
    Pp(PpBackend),
}

impl Tool {
    fn name(&self) -> &'static str {
        match self {
            Tool::Ec(_) => "ec-direct",
            Tool::Pp(_) => "platform-profile",
        }
    }

    fn read_state(&mut self) -> Result<(Option<Mode>, String), String> {
        match self {
            Tool::Ec(b) => b.read_state(),
            Tool::Pp(b) => b.read_state(),
        }
    }

    fn write_mode(&mut self, m: Mode) -> Result<(), String> {
        match self {
            Tool::Ec(b) => b.write_mode(m),
            Tool::Pp(b) => b.write_mode(m),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BackendSel {
    Auto,
    EcDirect,
    PlatformProfile,
}

fn open_tool(sel: BackendSel, mode_reg: u16) -> Result<Tool, String> {
    match sel {
        BackendSel::EcDirect => Ok(Tool::Ec(EcBackend::open(mode_reg)?)),
        BackendSel::PlatformProfile => {
            let dir = find_pp_dir().ok_or(
                "未找到支持三档性能模式的 platform-profile 设备（需要较新内核的 uniwill-laptop 驱动）。\n可改用: --backend ec-direct（配合 sudo modprobe ec_sys write_support=1）",
            )?;
            Ok(Tool::Pp(PpBackend { dir }))
        }
        BackendSel::Auto => {
            if let Some(dir) = find_pp_dir() {
                Ok(Tool::Pp(PpBackend { dir }))
            } else {
                Ok(Tool::Ec(EcBackend::open(mode_reg)?))
            }
        }
    }
}

// ---------------------------------------------------------------- CLI

#[derive(Clone)]
struct Args {
    backend: BackendSel,
    mode_reg: u16,
    dry_run: bool,
    json: bool,
    interval: u64,
    mux_offset: u16,
    hybrid_value: u8,
    dgpu_value: u8,
    positional: Vec<String>,
    auth_token: Option<String>,
    exit_on_stdin_close: bool,
}

fn parse_args() -> Result<Args, String> {
    parse_argv(std::env::args().skip(1).collect())
}

fn parse_argv(argv: Vec<String>) -> Result<Args, String> {
    let mut backend = BackendSel::Auto;
    let mut mode_reg = MODE_REG_DEFAULT;
    let mut dry_run = false;
    let mut json = false;
    let mut interval: u64 = 300;
    let mut mux_offset = MUX_OFFSET_DEFAULT;
    let mut hybrid_value = MUX_HYBRID_DEFAULT;
    let mut dgpu_value = MUX_DGPU_DEFAULT;
    let mut positional: Vec<String> = Vec::new();
    let mut auth_token = None;
    let mut exit_on_stdin_close = false;

    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        match a {
            "--backend" => {
                i += 1;
                let v = argv.get(i).ok_or("--backend 缺少参数")?;
                backend = match v.as_str() {
                    "auto" => BackendSel::Auto,
                    "ec-direct" | "ec" => BackendSel::EcDirect,
                    "platform-profile" | "pp" => BackendSel::PlatformProfile,
                    other => {
                        return Err(format!(
                            "未知后端: {other}（可选 auto|ec-direct|platform-profile）"
                        ))
                    }
                };
            }
            "--mode-reg" => {
                i += 1;
                let v = argv.get(i).ok_or("--mode-reg 缺少参数")?;
                mode_reg = parse_hex_u16(v).map_err(|_| format!("无效地址: {v}"))?;
            }
            "--mux-offset" => {
                i += 1;
                let v = argv.get(i).ok_or("--mux-offset 缺少参数")?;
                mux_offset = parse_hex_u16(v).map_err(|_| format!("无效偏移: {v}"))?;
            }
            "--hybrid-value" => {
                i += 1;
                let v = argv.get(i).ok_or("--hybrid-value 缺少参数")?;
                hybrid_value = parse_hex_u16(v).map_err(|_| format!("无效值: {v}"))? as u8;
            }
            "--dgpu-value" => {
                i += 1;
                let v = argv.get(i).ok_or("--dgpu-value 缺少参数")?;
                dgpu_value = parse_hex_u16(v).map_err(|_| format!("无效值: {v}"))? as u8;
            }
            "--auth-token" => {
                i += 1;
                let v = argv.get(i).ok_or("--auth-token 缺少参数")?;
                if v.is_empty() || v.starts_with("--") {
                    return Err("--auth-token 需要非空凭证，且不能以 -- 开头".into());
                }
                auth_token = Some(v.clone());
            }
            "--dry-run" => dry_run = true,
            "--exit-on-stdin-close" => exit_on_stdin_close = true,
            "--json" => json = true,
            "--interval" => {
                i += 1;
                let v = argv.get(i).ok_or("--interval 缺少参数")?;
                interval = v.parse().map_err(|_| format!("无效间隔: {v}（需要毫秒数，50-5000）"))?;
                if !(50..=5000).contains(&interval) {
                    return Err(format!("轮询间隔 {interval}ms 超出范围（50-5000）"));
                }
            }
            "-h" | "--help" | "help" => {
                print_help();
                std::process::exit(0);
            }
            "-v" | "--version" => {
                println!("firebat-cc 0.6.0");
                std::process::exit(0);
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }

    if auth_token.is_some() && positional.first().map(String::as_str) != Some("serve") {
        return Err("--auth-token 仅适用于 serve".into());
    }
    if exit_on_stdin_close && positional.first().map(String::as_str) != Some("serve") {
        return Err("--exit-on-stdin-close 仅适用于 serve".into());
    }
    Ok(Args {
        auth_token,
        exit_on_stdin_close,
        backend,
        mode_reg,
        dry_run,
        json,
        interval,
        mux_offset,
        hybrid_value,
        dgpu_value,
        positional,
    })
}

fn parse_hex_u16(s: &str) -> Result<u16, ()> {
    let t = s.trim();
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    u16::from_str_radix(t, 16).or_else(|_| s.trim().parse().map_err(|_| ()))
}

fn print_help() {
    println!(
        "firebat-cc 0.6.0 — Firebat GX 性能模式/显卡输出/键盘背光控制工具（Linux 逆向移植版）

用法:
  firebat-cc status                        查看当前性能模式
  firebat-cc mode <MODE>                   切换性能模式
  firebat-cc listen                        监听模式：检测性能模式键并按序切换
  firebat-cc serve [SOCKET] [--auth-token TOKEN]  守护模式：自动加载 ec_sys、开启写支持，退出时关闭
  firebat-cc ec-read <ADDR> [COUNT]        读取 EC 寄存器（调试用，16 进制地址）
  firebat-cc gpu status                    查看显卡输出模式（MUX）
  firebat-cc gpu hybrid                    切换为混合输出（重启生效）
  firebat-cc gpu dgpu                      切换为独显直连（重启生效）
  firebat-cc gpu raw <0-255>               直接写 MUX 字节（调试用，高危）
  firebat-cc uefi-dump                     十六进制转储 UEFI 厂商变量
  firebat-cc kb status                     查看键盘背光状态（类型/颜色/灯效）
  firebat-cc kb color <COLOR>              设置键盘背光颜色
  firebat-cc kb rainbow <on|off>           彩虹循环灯效开关
  firebat-cc kb brightness <0-100>         亮度（按当前颜色等比缩放）
  firebat-cc kb on | off                   开/关背光
  firebat-cc kb reset                      恢复固件默认颜色

COLOR 取值: RRGGBB 十六进制（如 ff8000）或名称:
  red 红  orange 橙  yellow 黄  green 绿  cyan 青  blue 蓝
  purple 紫  pink 粉  white 白

选项:
  --backend <auto|ec-direct|platform-profile>   性能模式后端选择（默认 auto）
  --mode-reg <hex>                              模式寄存器地址覆盖（默认 0x0751）
  --mux-offset <hex|dec>                        MUX 字段偏移覆盖（默认 98 = 0x62）
  --hybrid-value <n>                            混合输出对应值覆盖（默认 0）
  --dgpu-value <n>                              独显直连对应值覆盖（默认 1）
  --dry-run                                     只显示将要执行的操作，不实际写入
  --json                                        以 JSON 输出结果（前端/脚本集成用）
  --auth-token <TOKEN>                          serve 每条请求必须携带相同 token（默认不验证）
  --exit-on-stdin-close                         serve 在标准输入关闭时正常退出（GUI 生命周期绑定）
  --interval <ms>                               listen 轮询间隔（默认 300，50-5000）
  -h, --help                                    显示帮助
  -v, --version                                 显示版本

示例:
  sudo firebat-cc serve                   # 自动准备 ec_sys，Ctrl+C / SIGTERM 退出关闭写支持
  sudo modprobe ec_sys write_support=1     # ec-direct 性能模式后端需要
  sudo firebat-cc status
  sudo firebat-cc mode turbo
  sudo firebat-cc gpu dgpu                 # 独显直连，重启生效
  sudo firebat-cc kb color ff8000          # 键盘背光设为橙色
  sudo firebat-cc kb brightness 60         # 亮度 60%
  sudo firebat-cc uefi-dump                # 两种输出模式下各转储一次，diff 确认 MUX 偏移与取值

原理简述:
  性能模式写入 EC 0x0751 的模式位（办公 0xA0 / 均衡 0x20 / 狂暴 0x30）。
  显卡输出写入 UEFI 厂商变量 UniWillVariable/OemMagicVariable（GUID
  {{9f33f85c-13ca-4fd1-9c4a-96217722c593}}）中的 OemDisplayMode 字段，
  与 Windows 端控制中心同为「读整块-改一字段-写整块」，BIOS 重启时应用。
  键盘背光: R/G/B 写 EC 0x0769-0x076B，随后置 0x0767 的 APPLY_COLOR 位（0x20）
  使颜色生效；0x0767 bit7（0x80）为彩虹循环模式；白色单色键盘亮度走 0x1802。
  键盘寄存器语义与 tuxedo-drivers 内核驱动交叉验证一致。

风险提示: 直接写 EC / UEFI 变量属于底层硬件操作，请在真机上谨慎测试。"
    );
}

// ---------------------------------------------------------------- 子命令

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("错误: {e}\n（使用 -h 查看帮助）");
            std::process::exit(2);
        }
    };

    let Some(cmd) = args.positional.first().cloned() else {
        print_help();
        std::process::exit(2);
    };

    match cmd.as_str() {
        "status" => cmd_status(&args),
        "mode" => cmd_mode(&args),
        "listen" => cmd_listen(&args),
        "serve" => cmd_serve(&args),
        "ec-read" => cmd_ec_read(&args),
        "gpu" => cmd_gpu(&args),
        "kb" => cmd_kb(&args),
        "uefi-dump" => cmd_uefi_dump(&args),
        _ => {
            eprintln!("未知命令: {cmd}\n（使用 -h 查看帮助）");
            std::process::exit(2);
        }
    }
}

fn cmd_status(args: &Args) {
    if args.json {
        let out = match open_tool(args.backend, args.mode_reg) {
            Ok(mut tool) => match tool.read_state() {
                Ok((mode, detail)) => J::o(vec![
                    ("ok", J::B(true)),
                    (
                        "data",
                        J::o(vec![
                            ("mode", mode.map(|m| J::n(m.index() as f64)).unwrap_or(J::Null)),
                            ("name", J::s(mode.map(|m| m.zh()).unwrap_or("未知"))),
                            ("backend", J::s(tool.name())),
                            ("detail", J::s(&detail)),
                        ]),
                    ),
                ]),
                Err(e) => J::o(vec![("ok", J::B(false)), ("error", J::s(&e))]),
            },
            Err(e) => J::o(vec![("ok", J::B(false)), ("error", J::s(&e))]),
        };
        println!("{}", out.dump());
        return;
    }
    match open_tool(args.backend, args.mode_reg) {
        Ok(mut tool) => match tool.read_state() {
            Ok((mode, detail)) => {
                let cur = mode
                    .map(|m| format!("{} 档 - {}", m.index(), m.zh()))
                    .unwrap_or_else(|| "未知".into());
                println!("当前性能模式: {cur}");
                println!("  [{}] {detail}", tool.name());
                println!(
                    "  （EC 模式位含义: 办公 0xA0 / 均衡(游戏) 0x20 / 狂暴 0x30；Custom 与 Office 在 EC 中不可区分）"
                );
            }
            Err(e) => {
                eprintln!("错误: {e}");
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("错误: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_mode(args: &Args) {
    let Some(target_str) = args.positional.get(1) else {
        eprintln!("错误: mode 命令需要参数，例如: firebat-cc mode turbo\n（使用 -h 查看帮助）");
        std::process::exit(2);
    };
    let target = match Mode::parse(target_str) {
        Some(m) => m,
        None => {
            eprintln!(
                "错误: 未知模式 \"{target_str}\"（可选: 1|2|3|4、office|game|turbo|custom 或对应中文）"
            );
            std::process::exit(2);
        }
    };

    if args.json {
        if args.dry_run {
            let out = J::o(vec![
                ("ok", J::B(true)),
                ("data", J::o(vec![("dry_run", J::B(true)), ("mode", J::n(target.index() as f64)), ("name", J::s(target.zh()))])),
            ]);
            println!("{}", out.dump());
            return;
        }
        let out = match open_tool(args.backend, args.mode_reg) {
            Ok(mut tool) => {
                tool.write_mode(target).map(|_| {
                    J::o(vec![("mode", J::n(target.index() as f64)), ("name", J::s(target.zh()))])
                })
            }
            Err(e) => Err(e),
        };
        let out = match out {
            Ok(d) => J::o(vec![("ok", J::B(true)), ("data", d)]),
            Err(e) => J::o(vec![("ok", J::B(false)), ("error", J::s(&e))]),
        };
        println!("{}", out.dump());
        return;
    }

    if args.dry_run {
        println!(
            "[dry-run] 将切换到 {} 档 - {}（模式位 0x{:02X}，保留其他位）",
            target.index(),
            target.zh(),
            target.ec_bits()
        );
        return;
    }

    match open_tool(args.backend, args.mode_reg) {
        Ok(mut tool) => {
            let backend_name = tool.name();
            if let Err(e) = tool.write_mode(target) {
                eprintln!("错误: {e}");
                std::process::exit(1);
            }
            let detail = match tool.read_state() {
                Ok((cur, detail)) => {
                    let same = cur == Some(target)
                        // Custom 在 EC 中表现为 Office
                        || (cur == Some(Mode::Office) && target == Mode::Custom);
                    if !same {
                        eprintln!("警告: 切换后回读状态 ({cur:?}) 与目标 ({:?}) 不一致", target);
                    }
                    detail
                }
                Err(e) => format!("（回读失败: {e}）"),
            };
            println!("已切换到 {} 档 - {}（后端: {backend_name}）", target.index(), target.zh());
            println!("  {detail}");
        }
        Err(e) => {
            eprintln!("错误: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_ec_read(args: &Args) {
    let Some(addr_str) = args.positional.get(1) else {
        eprintln!("错误: ec-read 需要地址参数，例如: firebat-cc ec-read 0x751");
        std::process::exit(2);
    };
    if args.json {
        let count = args.positional.get(2).and_then(|s| s.parse().ok()).unwrap_or(1u16);
        let out = match EcBackend::open(args.mode_reg) {
            Ok(mut ec) => match api_ec_read(&mut ec, addr_str, count) {
                Ok(d) => J::o(vec![("ok", J::B(true)), ("data", d)]),
                Err(e) => J::o(vec![("ok", J::B(false)), ("error", J::s(&e))]),
            },
            Err(e) => J::o(vec![("ok", J::B(false)), ("error", J::s(&e))]),
        };
        println!("{}", out.dump());
        return;
    }
    let addr = match parse_hex_u16(addr_str) {
        Ok(a) => a,
        Err(_) => {
            eprintln!("错误: 无效地址 {addr_str}");
            std::process::exit(2);
        }
    };
    let count: u16 = args
        .positional
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .clamp(1, 64);

    let mut ec = match EcBackend::open(args.mode_reg) {
        Ok(ec) => ec,
        Err(e) => {
            eprintln!("错误: {e}");
            std::process::exit(1);
        }
    };
    for i in 0..count {
        let a = addr.wrapping_add(i);
        match ec.uw_read(a) {
            Ok(v) => println!("EC 0x{a:04X} = 0x{v:02X} (0b{:08b} {v:>3})", v),
            Err(e) => {
                eprintln!("错误: {e}");
                std::process::exit(1);
            }
        }
    }
}

// ---------------------------------------------------------------- 显卡输出（MUX）子命令

fn cmd_gpu(args: &Args) {
    let Some(sub) = args.positional.get(1).cloned() else {
        eprintln!("错误: gpu 命令需要子命令: status | hybrid | dgpu | raw <n>\n（使用 -h 查看帮助）");
        std::process::exit(2);
    };

    if args.json && sub != "raw" {
        let out = match api_gpu(args, &sub, args.dry_run) {
            Ok(d) => J::o(vec![("ok", J::B(true)), ("data", d)]),
            Err(e) => J::o(vec![("ok", J::B(false)), ("error", J::s(&e))]),
        };
        println!("{}", out.dump());
        return;
    }

    let uefi = match UefiBackend::open(args.mux_offset, args.hybrid_value, args.dgpu_value) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("错误: {e}");
            std::process::exit(1);
        }
    };

    match sub.as_str() {
        "status" => match (uefi.read(), uefi.read_mux()) {
            (Ok((attrs, payload)), Ok(mux)) => {
                println!("UEFI 变量: {}（负载 {} 字节，属性 0x{attrs:08X}）", uefi.path.display(), payload.len());
                println!("当前 MUX 字节 (OemDisplayMode @{}): 0x{mux:02X}", uefi.mux_offset);
                let name = if mux == uefi.hybrid_value {
                    "混合输出 (hybrid)"
                } else if mux == uefi.dgpu_value {
                    "独显直连 (dGPU only)"
                } else {
                    "未知值（可用 --hybrid-value/--dgpu-value 校准映射）"
                };
                println!("按当前值映射解释为: {name}");
                println!("已知字段:");
                for (off, size, name) in NVRAM_FIELDS {
                    if (off as usize + size as usize) <= payload.len() {
                        let v = &payload[off as usize..off as usize + size as usize];
                        let hexv: String = v.iter().map(|b| format!("{b:02X}")).collect();
                        println!("  +{off:<4} {name:<24} = 0x{hexv}");                    }
                }
                println!("注意: 0/1 与 混合/独显 的对应关系为推断值，可用 uefi-dump 做差分确认。");
            }
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("错误: {e}");
                std::process::exit(1);
            }
        },
        "hybrid" | "dgpu" => {
            let target_val = if sub == "hybrid" { uefi.hybrid_value } else { uefi.dgpu_value };
            let target_name = if sub == "hybrid" { "混合输出 (hybrid)" } else { "独显直连 (dGPU only)" };
            if args.dry_run {
                println!(
                    "[dry-run] 将把 UEFI 变量 {} 偏移 {} (OemDisplayMode) 写为 0x{target_val:02X}（{target_name}）",
                    uefi.var_name, uefi.mux_offset
                );
                return;
            }
            match uefi.read_mux() {
                Ok(cur) if cur == target_val => {
                    println!("当前已是 {target_name}（0x{cur:02X}），无需切换。");
                    return;
                }
                Ok(cur) => println!("当前 MUX 字节: 0x{cur:02X}"),
                Err(e) => {
                    eprintln!("错误: {e}");
                    std::process::exit(1);
                }
            }
            if let Err(e) = uefi.write_mux(target_val) {
                eprintln!("错误: {e}");
                std::process::exit(1);
            }
            println!("已写入 {target_name}（OemDisplayMode = 0x{target_val:02X}）。");
            println!("⚠️ 该设置由 BIOS 在下次开机时应用，请重启生效。重启前请勿再次修改。");
            println!("提示: 若重启后模式与预期不符，请用 --hybrid-value/--dgpu-value 调整映射，");
            println!("      或先在 Windows 控制中心/BIOS 中各设一次并用 uefi-dump 做差分对比。");
        }
        "raw" => {
            let Some(vstr) = args.positional.get(2) else {
                eprintln!("错误: gpu raw 需要 0-255 的数值参数");
                std::process::exit(2);
            };
            let val: u16 = match vstr.parse() {
                Ok(v) if v <= 255 => v,
                _ => {
                    eprintln!("错误: 无效值 {vstr}（需要 0-255）");
                    std::process::exit(2);
                }
            };
            if args.dry_run {
                println!("[dry-run] 将把 OemDisplayMode 写为 0x{val:02X}");
                return;
            }
            if let Err(e) = uefi.write_mux(val as u8) {
                eprintln!("错误: {e}");
                std::process::exit(1);
            }
            println!("已写入 OemDisplayMode = 0x{val:02X}。重启生效。");
        }
        other => {
            eprintln!("未知 gpu 子命令: {other}（可选: status | hybrid | dgpu | raw <n>）");
            std::process::exit(2);
        }
    }
}

fn cmd_uefi_dump(args: &Args) {
    let uefi = match UefiBackend::open(args.mux_offset, args.hybrid_value, args.dgpu_value) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("错误: {e}");
            std::process::exit(1);
        }
    };
    match uefi.read() {
        Ok((attrs, payload)) => {
            println!(
                "变量: {}（{}）",
                uefi.var_name,
                uefi.path.display()
            );
            println!("属性: 0x{attrs:08X}{}{}{}", 
                if attrs & 1 != 0 { " NON_VOLATILE" } else { "" },
                if attrs & 2 != 0 { " BOOTSERVICE" } else { "" },
                if attrs & 8 != 0 { " RUNTIME" } else { "" },
            );
            println!("负载: {} 字节", payload.len());
            println!("已知字段标注: MUX=OemDisplayMode 偏移 {}", uefi.mux_offset);
            for row in 0..payload.len().div_ceil(16) {
                let start = row * 16;
                let end = (start + 16).min(payload.len());
                let hexs: String = payload[start..end]
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                let ascii: String = payload[start..end]
                    .iter()
                    .map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' })
                    .collect();
                let mark = if (uefi.mux_offset as usize) >= start && (uefi.mux_offset as usize) < end {
                    " <- MUX"
                } else {
                    ""
                };
                println!("  {start:04X}: {hexs:<47} |{ascii}|{mark}");
            }
            println!();
            println!("差分确认法: 在 Windows 控制中心分别设为 混合输出/独显直连（或 BIOS 中切换），");
            println!("每次重启到 Linux 后运行本命令并对比 OemDisplayMode 字节的取值。");
        }
        Err(e) => {
            eprintln!("错误: {e}");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------- 键盘背光子命令

fn parse_kb_color(s: &str) -> Option<(u8, u8, u8)> {
    let lower = s.trim().to_ascii_lowercase();
    let named = match lower.as_str() {
        "red" | "红" => Some((0xFF, 0x00, 0x00)),
        "orange" | "橙" => Some((0xFF, 0x40, 0x00)),
        "yellow" | "黄" => Some((0xFF, 0xC8, 0x00)),
        "green" | "绿" => Some((0x00, 0xFF, 0x00)),
        "cyan" | "青" => Some((0x00, 0xC8, 0xFF)),
        "blue" | "蓝" => Some((0x00, 0x40, 0xFF)),
        "purple" | "紫" => Some((0x80, 0x00, 0xFF)),
        "pink" | "粉" => Some((0xFF, 0x40, 0x80)),
        "white" | "白" => Some((0xFF, 0xFF, 0xFF)),
        _ => None,
    };
    if let Some(c) = named {
        return Some(c);
    }
    let hex = lower.trim_start_matches('#');
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let v = u32::from_str_radix(hex, 16).ok()?;
        return Some(((v >> 16) as u8, (v >> 8) as u8, v as u8));
    }
    None
}

fn open_ec_for_kb(args: &Args) -> Result<EcBackend, String> {
    if args.backend == BackendSel::PlatformProfile {
        return Err(
            "kb 命令仅支持 ec-direct 后端（platform-profile 接口不暴露键盘背光寄存器）".into(),
        );
    }
    EcBackend::open(args.mode_reg)
}

/// 读-改-写 0x0767 模式寄存器（该地址与 USB PoweShare 等共用，必须保留其他位）
fn kb_mode_rmw(ec: &mut EcBackend, set: u8, clear: u8) -> Result<u8, String> {
    let cur = ec.uw_read(KBD_BL_RGB_MODE)?;
    let next = (cur | set) & !clear;
    ec.uw_write(KBD_BL_RGB_MODE, next)?;
    Ok(next)
}

fn cmd_kb(args: &Args) {
    let Some(sub) = args.positional.get(1).cloned() else {
        eprintln!("错误: kb 命令需要子命令: status | color | rainbow | brightness | on | off | reset\n（使用 -h 查看帮助）");
        std::process::exit(2);
    };

    if args.json {
        let out = match open_ec_for_kb(args) {
            Ok(mut ec) => match api_kb(&mut ec, &sub, args.positional.get(2).map(|s| s.as_str()), args.dry_run) {
                Ok(d) => J::o(vec![("ok", J::B(true)), ("data", d)]),
                Err(e) => J::o(vec![("ok", J::B(false)), ("error", J::s(&e))]),
            },
            Err(e) => J::o(vec![("ok", J::B(false)), ("error", J::s(&e))]),
        };
        println!("{}", out.dump());
        return;
    }

    match sub.as_str() {
        "status" => {
            let mut ec = open_ec_for_kb(args).unwrap_or_else(|e| die(&e));
            match kb_status(&mut ec) {
                Ok(()) => {}
                Err(e) => die(&e),
            }
        }
        "color" => {
            let Some(cstr) = args.positional.get(2) else {
                eprintln!("错误: kb color 需要颜色参数（RRGGBB 或名称，如 ff8000 / orange）");
                std::process::exit(2);
            };
            let (r, g, b) = match parse_kb_color(cstr) {
                Some(c) => c,
                None => {
                    eprintln!("错误: 无法解析颜色 \"{cstr}\"（支持 RRGGBB 十六进制或 red/blue/white 等名称）");
                    std::process::exit(2);
                }
            };
            if args.dry_run {
                println!(
                    "[dry-run] 将写 EC 0x0769/0x076A/0x076B = 0x{:02X}/0x{:02X}/0x{:02X}（通道值域 0-50，已从 0-255 转换）",
                    kb_convert_channel(r),
                    kb_convert_channel(g),
                    kb_convert_channel(b)
                );
                println!("[dry-run] 并置 0x0767 的 APPLY_COLOR 位（0x20，读-改-写保留其他位）");
                return;
            }
            let mut ec = open_ec_for_kb(args).unwrap_or_else(|e| die(&e));
            if let Err(e) = (|| -> Result<(), String> {
                let (r2, g2, b2) = (kb_convert_channel(r), kb_convert_channel(g), kb_convert_channel(b));
                if (r2, g2, b2) == (0, 0, 0) {
                    // 全零是 EC 特殊情况（会写入默认值），改为直接把亮度级归零
                    let cur = ec.uw_read(KBD_BL_STATUS)?;
                    ec.uw_write(KBD_BL_STATUS, (cur & !KBD_BL_STATUS_BRI_MASK) | KBD_BL_STATUS_RESET)?;
                    println!("颜色全为零：已将亮度级归零（背光熄灭）");
                    return Ok(());
                }
                ec.uw_write(KBD_BL_RGB_RED, r2)?;
                ec.uw_write(KBD_BL_RGB_GREEN, g2)?;
                ec.uw_write(KBD_BL_RGB_BLUE, b2)?;
                let mode = kb_mode_rmw(&mut ec, KBD_BL_RGB_MODE_APPLY, 0)?;
                if mode & KBD_BL_RGB_MODE_RAINBOW != 0 {
                    println!("注意: 彩虹循环模式（0x0767 bit7）当前开启，静态颜色会被彩虹覆盖；可用 kb rainbow off 关闭。");
                }
                // 回读校验
                let back = (
                    ec.uw_read(KBD_BL_RGB_RED)?,
                    ec.uw_read(KBD_BL_RGB_GREEN)?,
                    ec.uw_read(KBD_BL_RGB_BLUE)?,
                );
                if back != (r2, g2, b2) {
                    return Err(format!(
                        "回读校验失败: 0x{:02X}/0x{:02X}/0x{:02X} != 0x{r2:02X}/0x{g2:02X}/0x{b2:02X}",
                        back.0, back.1, back.2
                    ));
                }
                Ok(())
            })() {
                die(&e);
            }
            let (r2, g2, b2) = (kb_convert_channel(r), kb_convert_channel(g), kb_convert_channel(b));
            println!(
                "已设置键盘背光颜色: #{r:02X}{g:02X}{b:02X}（EC 通道 0x{r2:02X}/0x{g2:02X}/0x{b2:02X}，值域 0-50）"
            );
        }
        "rainbow" => {
            let on = match args.positional.get(2).map(|s| s.as_str()) {
                Some("on") | Some("1") | Some("开") => true,
                Some("off") | Some("0") | Some("关") => false,
                _ => {
                    eprintln!("错误: kb rainbow 需要 on 或 off 参数");
                    std::process::exit(2);
                }
            };
            if args.dry_run {
                println!(
                    "[dry-run] 将{} 0x0767 的 RAINBOW 位（0x80）并置 APPLY_COLOR（读-改-写保留其他位）",
                    if on { "置位" } else { "清除" }
                );
                return;
            }
            let mut ec = open_ec_for_kb(args).unwrap_or_else(|e| die(&e));
            if let Err(e) = kb_mode_rmw(
                &mut ec,
                if on { KBD_BL_RGB_MODE_RAINBOW | KBD_BL_RGB_MODE_APPLY } else { KBD_BL_RGB_MODE_APPLY },
                if on { 0 } else { KBD_BL_RGB_MODE_RAINBOW },
            ) {
                die(&e);
            }
            println!("彩虹循环灯效已{}", if on { "开启" } else { "关闭" });
        }
        "brightness" => {
            let Some(pstr) = args.positional.get(2) else {
                eprintln!("错误: kb brightness 需要 0-100 的百分比参数");
                std::process::exit(2);
            };
            let pct: u32 = match pstr.parse() {
                Ok(v) if v <= 100 => v,
                _ => {
                    eprintln!("错误: 无效亮度 \"{pstr}\"（需要 0-100）");
                    std::process::exit(2);
                }
            };
            if args.dry_run {
                println!(
                    "[dry-run] 将按 {pct}% 映射到 0x078C bit5-7 亮度级（0-{}），并置 0x10 应用位（读-改-写保留其他位）",
                    KBD_BL_STATUS_BRI_MAX
                );
                return;
            }
            let mut ec = open_ec_for_kb(args).unwrap_or_else(|e| die(&e));
            if let Err(e) = (|| -> Result<(), String> {
                let white_only = ec.uw_read(KBD_BL_STATUS)? & KBD_BL_STATUS_WHITE_ONLY != 0;
                if white_only {
                    let v = (pct * 255 / 100) as u8;
                    ec.uw_write(KBD_BL_WHITE_BRI_IMM, v)?;
                    println!("白色单色键盘: 已写 0x1802 立即亮度 = 0x{v:02X}（{pct}%，值域为推断）");
                    return Ok(());
                }
                // 1-zone RGB：亮度级 0-4 存于 0x078C bit5-7，0x10 应用
                // （u32 运算防溢出：pct*4+50 最大 450，u8 会回绕——v0.3.2 真机 bug）
                let level = ((pct * (KBD_BL_STATUS_BRI_MAX as u32) + 50) / 100).min(4) as u8;
                let cur = ec.uw_read(KBD_BL_STATUS)?;
                let next = (cur & !KBD_BL_STATUS_BRI_MASK & !KBD_BL_STATUS_DISABLED) | (level << 5) | KBD_BL_STATUS_RESET;
                ec.uw_write(KBD_BL_STATUS, next)?;
                println!("亮度 {pct}%: 0x078C 0x{cur:02X} → 0x{next:02X}（亮度级 {level}/{}）", KBD_BL_STATUS_BRI_MAX);
                Ok(())
            })() {
                die(&e);
            }
        }
        "on" => {
            if args.dry_run {
                println!("[dry-run] 将清除 0x078C bit1（禁用位）重新点亮背光；若当前颜色全零则先用固件默认色");
                return;
            }
            let mut ec = open_ec_for_kb(args).unwrap_or_else(|e| die(&e));
            if let Err(e) = (|| -> Result<(), String> {
                // 颜色全零时先恢复默认色，否则点亮后仍是黑的
                let (r, g, b) = (
                    ec.uw_read(KBD_BL_RGB_RED)?,
                    ec.uw_read(KBD_BL_RGB_GREEN)?,
                    ec.uw_read(KBD_BL_RGB_BLUE)?,
                );
                if (r, g, b) == (0, 0, 0) {
                    let (dr, dg, db) = (
                        ec.uw_read(KBD_BL_DEFAULT_RED)?,
                        ec.uw_read(KBD_BL_DEFAULT_GREEN)?,
                        ec.uw_read(KBD_BL_DEFAULT_BLUE)?,
                    );
                    if (dr, dg, db) == (0, 0, 0) {
                        // 默认色也全零：用白
                        ec.uw_write(KBD_BL_RGB_RED, kb_convert_channel(0xFF))?;
                        ec.uw_write(KBD_BL_RGB_GREEN, kb_convert_channel(0xFF))?;
                        ec.uw_write(KBD_BL_RGB_BLUE, kb_convert_channel(0xFF))?;
                    } else {
                        ec.uw_write(KBD_BL_RGB_RED, dr)?;
                        ec.uw_write(KBD_BL_RGB_GREEN, dg)?;
                        ec.uw_write(KBD_BL_RGB_BLUE, db)?;
                    }
                    kb_mode_rmw(&mut ec, KBD_BL_RGB_MODE_APPLY, 0)?;
                }
                // 清除禁用位（bit1）并保持亮度级
                let cur = ec.uw_read(KBD_BL_STATUS)?;
                ec.uw_write(KBD_BL_STATUS, cur & !KBD_BL_STATUS_DISABLED)?;
                println!("背光已开启（0x078C 0x{cur:02X} → 0x{:02X}，bit1 禁用位已清除）", cur & !KBD_BL_STATUS_DISABLED);
                Ok(())
            })() {
                die(&e);
            }
        }
        "off" => {
            if args.dry_run {
                println!("[dry-run] 将置位 0x078C bit1（禁用位，tuxedo uniwill_write_kbd_bl_enable 反逻辑）");
                return;
            }
            let mut ec = open_ec_for_kb(args).unwrap_or_else(|e| die(&e));
            if let Err(e) = (|| -> Result<(), String> {
                let cur = ec.uw_read(KBD_BL_STATUS)?;
                ec.uw_write(KBD_BL_STATUS, cur | KBD_BL_STATUS_DISABLED)?;
                println!(
                    "背光已关闭（0x078C 0x{cur:02X} → 0x{:02X}，颜色寄存器保持不变）",
                    cur | KBD_BL_STATUS_DISABLED
                );
                Ok(())
            })() {
                die(&e);
            }
        }
        "reset" => {
            if args.dry_run {
                println!("[dry-run] 将把固件默认色 0x076C-0x076E 抄写回 0x0769-0x076B 并置 APPLY_COLOR（恢复默认颜色）");
                return;
            }
            let mut ec = open_ec_for_kb(args).unwrap_or_else(|e| die(&e));
            if let Err(e) = (|| -> Result<(), String> {
                let (dr, dg, db) = (
                    ec.uw_read(KBD_BL_DEFAULT_RED)?,
                    ec.uw_read(KBD_BL_DEFAULT_GREEN)?,
                    ec.uw_read(KBD_BL_DEFAULT_BLUE)?,
                );
                // 说明：0x078C 的 0x10 与 0x0767 的 APPLY_COLOR 均为一次性触发位，
                // EC 消费后自动清零（真机验证）。单独 OR 0x10 只会重新应用当前设置，
                // 不产生可见变化；「重置」的正确路径是把默认色抄回颜色寄存器再 APPLY。
                ec.uw_write(KBD_BL_RGB_RED, dr)?;
                ec.uw_write(KBD_BL_RGB_GREEN, dg)?;
                ec.uw_write(KBD_BL_RGB_BLUE, db)?;
                kb_mode_rmw(&mut ec, KBD_BL_RGB_MODE_APPLY, 0)?;
                let back = (
                    ec.uw_read(KBD_BL_RGB_RED)?,
                    ec.uw_read(KBD_BL_RGB_GREEN)?,
                    ec.uw_read(KBD_BL_RGB_BLUE)?,
                );
                if back != (dr, dg, db) {
                    return Err(format!(
                        "回读校验失败: 0x{:02X}/0x{:02X}/0x{:02X} != 0x{dr:02X}/0x{dg:02X}/0x{db:02X}",
                        back.0, back.1, back.2
                    ));
                }
                println!(
                    "已恢复固件默认色 {dr}/{dg}/{db}（0-50 值域，来自 0x076C-0x076E）；亮度级保持不变。"
                );
                Ok(())
            })() {
                die(&e);
            }
        }
        other => {
            eprintln!("未知 kb 子命令: {other}（可选: status | color | rainbow | brightness | on | off | reset）");
            std::process::exit(2);
        }
    }
}

fn kb_status(ec: &mut EcBackend) -> Result<(), String> {
    let status = ec.uw_read(KBD_BL_STATUS)?;
    let feat1 = ec.uw_read(FEATURES_1)?;
    let mode = ec.uw_read(KBD_BL_RGB_MODE)?;
    let (r, g, b) = (
        ec.uw_read(KBD_BL_RGB_RED)?,
        ec.uw_read(KBD_BL_RGB_GREEN)?,
        ec.uw_read(KBD_BL_RGB_BLUE)?,
    );
    let (dr, dg, db) = (
        ec.uw_read(KBD_BL_DEFAULT_RED)?,
        ec.uw_read(KBD_BL_DEFAULT_GREEN)?,
        ec.uw_read(KBD_BL_DEFAULT_BLUE)?,
    );

    println!("键盘背光状态（ec-direct，Uniwill ITE EC）:");
    if status & KBD_BL_STATUS_WHITE_ONLY != 0 {
        println!("  键盘类型:   白色单色（0x078C bit0 置位），亮度寄存器 0x1802");
    } else if feat1 & FEATURES_1_BIT_1ZONE_RGB != 0 {
        println!("  键盘类型:   RGB（0x0766 bit2 置位，单区/1-zone RGB，已真机验证）");
    } else {
        println!("  键盘类型:   未知（0x078C=0x{status:02X} 0x0766=0x{feat1:02X}，两个特性位均未置位）");
    }
    let level = (status & KBD_BL_STATUS_BRI_MASK) >> 5;
    println!(
        "  背光开关:   {}（bit1 禁用位 = {}）",
        if status & KBD_BL_STATUS_DISABLED != 0 { "关" } else { "开" },
        if status & KBD_BL_STATUS_DISABLED != 0 { "1" } else { "0" }
    );
    println!("  亮度级:     {level}/{}（0x078C bit5-7，0x078C=0x{status:02X}）", KBD_BL_STATUS_BRI_MAX);
    println!(
        "  彩虹循环:   {}（0x0767=0x{mode:02X}，bit7 RAINBOW）",
        if mode & KBD_BL_RGB_MODE_RAINBOW != 0 { "开" } else { "关" }
    );
    println!(
        "  APPLY_COLOR: {}（bit5）",
        if mode & KBD_BL_RGB_MODE_APPLY != 0 { "已置位" } else { "未置位" }
    );
    println!(
        "  当前颜色:   #{r:02X}{g:02X}{b:02X}（EC 通道值域 0-50：R={r} G={g} B={b} @0x0769-0x076B）"
    );
    println!(
        "  固件默认色: {dr}/{dg}/{db}（0-50 值域 @0x076C-0x076E）"
    );
    println!("  提示: 彩虹开启时静态颜色被覆盖；设置颜色请用 kb color（自动做 0-255→0-50 转换）。");
    Ok(())
}

fn die(e: &str) -> ! {
    eprintln!("错误: {e}");
    std::process::exit(1);
}

// ---------------------------------------------------------------- 监听模式（性能模式键）

/// 性能模式热键循环顺序：办公 → 游戏(均衡) → 狂暴 → 循环（自定义档等同办公）
fn next_mode(m: Mode) -> Mode {
    match m {
        Mode::Office | Mode::Custom => Mode::Game,
        Mode::Game => Mode::Turbo,
        Mode::Turbo => Mode::Office,
    }
}

fn cmd_listen(args: &Args) {
    if args.backend == BackendSel::PlatformProfile {
        die("listen 命令仅支持 ec-direct 后端（需要轮询 EC 模式寄存器原始值）");
    }
    let mut ec = EcBackend::open(args.mode_reg).unwrap_or_else(|e| die(&e));
    let interval = args.interval;

    let (mode, detail) = ec.read_state().unwrap_or_else(|e| die(&e));
    let mut current = mode.unwrap_or(Mode::Office);
    let mut last_bits = ec.uw_read(args.mode_reg).unwrap_or_else(|e| die(&e));

    println!("监听模式已启动（Ctrl+C 退出）");
    println!("  轮询: EC 0x{:04X} 每 {interval}ms 一次", args.mode_reg);
    println!("  当前模式: {} 档 - {}（{}）", current.index(), current.zh(), detail);
    println!("  切换顺序: 办公(1) → 游戏(均衡)(2) → 狂暴(3) → 办公 循环");
    if args.dry_run {
        println!("  [dry-run] 检测到按键后只报告、不写入。");
    }
    println!("  提示: 每次检测到模式寄存器被外部改变（即按键按下），都会强制写入循环中的下一档。");
    println!();

    loop {
        std::thread::sleep(Duration::from_millis(interval));
        let bits = match ec.uw_read(args.mode_reg) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("警告: EC 读取失败: {e}");
                continue;
            }
        };
        if bits == last_bits {
            continue;
        }

        // 模式寄存器被外部改变 = 性能模式键按下（EC 固件自行改档）
        let detected = mode_from_bits(bits);
        let target = next_mode(current);

        if args.dry_run {
            let det = detected
                .map(|m| format!("{} 档 - {}", m.index(), m.zh()))
                .unwrap_or_else(|| format!("未知(0x{bits:02X})"));
            println!(
                "检测到按键: 0x{last_bits:02X} → 0x{bits:02X}（EC 切到了 {det}）；目标下一档: {} 档 - {}（dry-run 不写入）",
                target.index(),
                target.zh()
            );
            last_bits = bits;
            if let Some(m) = detected {
                current = m;
            }
            continue;
        }

        match ec.write_mode(target) {
            Ok(()) => {
                current = target;
                println!("检测到性能模式键 → 已切换到: {} 档 - {}", target.index(), target.zh());
            }
            Err(e) => {
                eprintln!("错误: 写入下一档失败: {e}");
                if let Some(m) = detected {
                    current = m;
                }
            }
        }
        // 以写入后的实际寄存器值为新基线（我们的写入不算外部变化）
        last_bits = ec.uw_read(args.mode_reg).unwrap_or(bits);
    }
}

// ---------------------------------------------------------------- 最小 JSON（零依赖）

#[derive(Clone, Debug, PartialEq)]
enum J {
    Null,
    B(bool),
    N(f64),
    S(String),
    A(Vec<J>),
    O(Vec<(String, J)>),
}

impl J {
    fn s(v: &str) -> J {
        J::S(v.to_string())
    }
    fn n(v: impl Into<f64>) -> J {
        J::N(v.into())
    }
    fn o(pairs: Vec<(&str, J)>) -> J {
        J::O(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }
    fn get(&self, k: &str) -> Option<&J> {
        if let J::O(m) = self {
            m.iter().find(|(key, _)| key == k).map(|(_, v)| v)
        } else {
            None
        }
    }
    fn as_str(&self) -> Option<&str> {
        if let J::S(s) = self {
            Some(s)
        } else {
            None
        }
    }
    fn as_bool(&self) -> Option<bool> {
        if let J::B(b) = self {
            Some(*b)
        } else {
            None
        }
    }

    fn dump(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }
    fn write(&self, out: &mut String) {
        match self {
            J::Null => out.push_str("null"),
            J::B(b) => out.push_str(if *b { "true" } else { "false" }),
            J::N(v) => {
                if v.fract() == 0.0 && v.abs() < 9e15 {
                    out.push_str(&format!("{}", *v as i64));
                } else {
                    out.push_str(&format!("{v}"));
                }
            }
            J::S(s) => {
                out.push('"');
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            J::A(items) => {
                out.push('[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    it.write(out);
                }
                out.push(']');
            }
            J::O(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    J::s(k).write(out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn jparse(s: &str) -> Result<J, String> {
    let b: Vec<char> = s.chars().collect();
    let (v, _) = jp_value(&b, 0)?;
    Ok(v)
}

fn jp_ws(b: &[char], mut i: usize) -> usize {
    while i < b.len() && b[i].is_whitespace() {
        i += 1;
    }
    i
}

fn jp_value(b: &[char], i: usize) -> Result<(J, usize), String> {
    let i = jp_ws(b, i);
    if i >= b.len() {
        return Err("JSON 意外结束".into());
    }
    match b[i] {
        '{' => {
            let mut obj: Vec<(String, J)> = Vec::new();
            let mut i = jp_ws(b, i + 1);
            if i < b.len() && b[i] == '}' {
                return Ok((J::O(obj), i + 1));
            }
            loop {
                i = jp_ws(b, i);
                let (k, ni) = jp_value(b, i)?;
                let key = k.as_str().ok_or("对象键必须是字符串")?.to_string();
                i = jp_ws(b, ni);
                if i >= b.len() || b[i] != ':' {
                    return Err("JSON 对象缺少 :".into());
                }
                let (v, ni) = jp_value(b, i + 1)?;
                obj.push((key, v));
                i = jp_ws(b, ni);
                if i >= b.len() {
                    return Err("JSON 对象未闭合".into());
                }
                match b[i] {
                    ',' => i += 1,
                    '}' => return Ok((J::O(obj), i + 1)),
                    _ => return Err("JSON 对象缺少 }".into()),
                }
            }
        }
        '[' => {
            let mut arr = Vec::new();
            let mut i = jp_ws(b, i + 1);
            if i < b.len() && b[i] == ']' {
                return Ok((J::A(arr), i + 1));
            }
            loop {
                let (v, ni) = jp_value(b, i)?;
                arr.push(v);
                i = jp_ws(b, ni);
                if i >= b.len() {
                    return Err("JSON 数组未闭合".into());
                }
                match b[i] {
                    ',' => i += 1,
                    ']' => return Ok((J::A(arr), i + 1)),
                    _ => return Err("JSON 数组缺少 ]".into()),
                }
            }
        }
        '"' => {
            let mut s = String::new();
            let mut i = i + 1;
            while i < b.len() {
                match b[i] {
                    '"' => return Ok((J::S(s), i + 1)),
                    '\\' => {
                        i += 1;
                        if i >= b.len() {
                            return Err("JSON 字符串转义未闭合".into());
                        }
                        match b[i] {
                            'n' => s.push('\n'),
                            't' => s.push('\t'),
                            'r' => s.push('\r'),
                            'u' => {
                                let digits = b.get(i + 1..i + 5).ok_or("JSON \\u 转义未闭合")?;
                                let hex: String = digits.iter().collect();
                                let cp = u32::from_str_radix(&hex, 16)
                                    .map_err(|_| "JSON \\u 转义无效".to_string())?;
                                if let Some(c) = char::from_u32(cp) {
                                    s.push(c);
                                }
                                i += 4;
                            }
                            c => s.push(c),
                        }
                        i += 1;
                    }
                    c => {
                        s.push(c);
                        i += 1;
                    }
                }
            }
            Err("JSON 字符串未闭合".into())
        }
        't' if b[i..].starts_with(&['t', 'r', 'u', 'e']) => Ok((J::B(true), i + 4)),
        'f' if b[i..].starts_with(&['f', 'a', 'l', 's', 'e']) => Ok((J::B(false), i + 5)),
        'n' if b[i..].starts_with(&['n', 'u', 'l', 'l']) => Ok((J::Null, i + 4)),
        _ => {
            let start = i;
            let mut i = i;
            while i < b.len() && (b[i].is_ascii_digit() || matches!(b[i], '-' | '+' | '.' | 'e' | 'E')) {
                i += 1;
            }
            let txt: String = b[start..i].iter().collect();
            txt.parse::<f64>().map(J::N).map(|v| (v, i)).map_err(|_| format!("JSON 数字无效: {txt}"))
        }
    }
}

// ---------------------------------------------------------------- API 层（--json 与 serve 共用）

fn api_status_ec(ec: &mut EcBackend) -> Result<J, String> {
    let (mode, detail) = ec.read_state()?;
    let cur = mode.unwrap_or(Mode::Office);
    Ok(J::o(vec![
        ("mode", mode.map(|m| J::n(m.index() as f64)).unwrap_or(J::Null)),
        ("name", J::s(cur.zh())),
        ("bits", J::s(&format!("{:02X}", ec.uw_read(MODE_REG_DEFAULT).unwrap_or(0)))),
        ("backend", J::s("ec-direct")),
        ("detail", J::s(&detail)),
    ]))
}

fn api_mode_set_ec(ec: &mut EcBackend, value: &str) -> Result<J, String> {
    let target = Mode::parse(value).ok_or_else(|| format!("未知模式: {value}"))?;
    ec.write_mode(target)?;
    Ok(J::o(vec![
        ("mode", J::n(target.index() as f64)),
        ("name", J::s(target.zh())),
        ("bits", J::s(&format!("{:02X}", target.ec_bits()))),
    ]))
}

fn api_gpu(args: &Args, action: &str, dry_run: bool) -> Result<J, String> {
    let uefi = UefiBackend::open(args.mux_offset, args.hybrid_value, args.dgpu_value)?;
    match action {
        "status" => {
            let (_, payload) = uefi.read()?;
            let mux = uefi.read_mux()?;
            let interp = if mux == args.hybrid_value {
                "hybrid"
            } else if mux == args.dgpu_value {
                "dgpu"
            } else {
                "unknown"
            };
            Ok(J::o(vec![
                ("var", J::s(uefi.var_name)),
                ("mux_raw", J::n(mux as f64)),
                ("interpreted", J::s(interp)),
                ("payload_len", J::n(payload.len() as f64)),
            ]))
        }
        "hybrid" | "dgpu" => {
            let target = if action == "hybrid" { args.hybrid_value } else { args.dgpu_value };
            if dry_run {
                return Ok(J::o(vec![("dry_run", J::B(true)), ("would_write", J::n(target as f64))]));
            }
            let cur = uefi.read_mux()?;
            if cur != target {
                uefi.write_mux(target)?;
            }
            Ok(J::o(vec![
                ("written", J::n(target as f64)),
                ("unchanged", J::B(cur == target)),
                ("reboot_required", J::B(true)),
            ]))
        }
        other => Err(format!("未知 gpu 动作: {other}（status|hybrid|dgpu）")),
    }
}

fn api_kb(ec: &mut EcBackend, action: &str, value: Option<&str>, dry_run: bool) -> Result<J, String> {
    match action {
        "status" => {
            let status = ec.uw_read(KBD_BL_STATUS)?;
            let mode = ec.uw_read(KBD_BL_RGB_MODE)?;
            let (r, g, b) = (
                ec.uw_read(KBD_BL_RGB_RED)?,
                ec.uw_read(KBD_BL_RGB_GREEN)?,
                ec.uw_read(KBD_BL_RGB_BLUE)?,
            );
            Ok(J::o(vec![
                ("white_only", J::B(status & KBD_BL_STATUS_WHITE_ONLY != 0)),
                ("disabled", J::B(status & KBD_BL_STATUS_DISABLED != 0)),
                ("brightness_level", J::n(((status & KBD_BL_STATUS_BRI_MASK) >> 5) as f64)),
                ("brightness_max", J::n(KBD_BL_STATUS_BRI_MAX as f64)),
                ("rainbow", J::B(mode & KBD_BL_RGB_MODE_RAINBOW != 0)),
                ("color", J::o(vec![
                    ("r", J::n(r as f64)),
                    ("g", J::n(g as f64)),
                    ("b", J::n(b as f64)),
                ])),
            ]))
        }
        "color" => {
            let (r, g, b) = parse_kb_color(value.ok_or("kb color 需要颜色参数")?)
                .ok_or_else(|| format!("无法解析颜色"))?;
            let (r2, g2, b2) = (kb_convert_channel(r), kb_convert_channel(g), kb_convert_channel(b));
            if dry_run {
                return Ok(J::o(vec![("dry_run", J::B(true)), ("r", J::n(r2 as f64)), ("g", J::n(g2 as f64)), ("b", J::n(b2 as f64))]));
            }
            ec.uw_write(KBD_BL_RGB_RED, r2)?;
            ec.uw_write(KBD_BL_RGB_GREEN, g2)?;
            ec.uw_write(KBD_BL_RGB_BLUE, b2)?;
            kb_mode_rmw(ec, KBD_BL_RGB_MODE_APPLY, 0)?;
            Ok(J::o(vec![("r", J::n(r2 as f64)), ("g", J::n(g2 as f64)), ("b", J::n(b2 as f64))]))
        }
        "rainbow" => {
            let on = matches!(value, Some("on") | Some("1") | Some("开"));
            if dry_run {
                return Ok(J::o(vec![("dry_run", J::B(true)), ("rainbow", J::B(on))]));
            }
            kb_mode_rmw(
                ec,
                if on { KBD_BL_RGB_MODE_RAINBOW | KBD_BL_RGB_MODE_APPLY } else { KBD_BL_RGB_MODE_APPLY },
                if on { 0 } else { KBD_BL_RGB_MODE_RAINBOW },
            )?;
            Ok(J::o(vec![("rainbow", J::B(on))]))
        }
        "brightness" => {
            let pct: u32 = value.ok_or("kb brightness 需要参数")?.parse().map_err(|_| "无效亮度")?;
            if pct > 100 {
                return Err("亮度需要 0-100".into());
            }
            if dry_run {
                return Ok(J::o(vec![("dry_run", J::B(true)), ("percent", J::n(pct as f64))]));
            }
            let white_only = ec.uw_read(KBD_BL_STATUS)? & KBD_BL_STATUS_WHITE_ONLY != 0;
            if white_only {
                let v = (pct * 255 / 100) as u8;
                ec.uw_write(KBD_BL_WHITE_BRI_IMM, v)?;
                return Ok(J::o(vec![("white_brightness", J::n(v as f64))]));
            }
            let level = ((pct * (KBD_BL_STATUS_BRI_MAX as u32) + 50) / 100).min(4) as u8;
            let cur = ec.uw_read(KBD_BL_STATUS)?;
            let next = (cur & !KBD_BL_STATUS_BRI_MASK & !KBD_BL_STATUS_DISABLED) | (level << 5) | KBD_BL_STATUS_RESET;
            ec.uw_write(KBD_BL_STATUS, next)?;
            Ok(J::o(vec![("percent", J::n(pct as f64)), ("level", J::n(level as f64))]))
        }
        "on" | "off" => {
            let enable = action == "on";
            if dry_run {
                return Ok(J::o(vec![("dry_run", J::B(true)), ("enabled", J::B(enable))]));
            }
            let cur = ec.uw_read(KBD_BL_STATUS)?;
            let next = if enable { cur & !KBD_BL_STATUS_DISABLED } else { cur | KBD_BL_STATUS_DISABLED };
            ec.uw_write(KBD_BL_STATUS, next)?;
            Ok(J::o(vec![("enabled", J::B(enable))]))
        }
        "reset" => {
            if dry_run {
                return Ok(J::o(vec![("dry_run", J::B(true)), ("action", J::s("reset"))]));
            }
            // 注意：0x078C 的 0x10 与 0x0767 的 APPLY_COLOR 都是一次性触发位，EC 消费后
            // 自动清零；单独置 0x10 只是「重新应用当前设置」，颜色不会有任何变化。
            // 与 CLI `kb reset` 保持一致：把固件默认色 0x076C-0x076E 抄回 0x0769-0x076B
            // 再 APPLY，才是真正的“恢复默认颜色”。
            let (dr, dg, db) = (
                ec.uw_read(KBD_BL_DEFAULT_RED)?,
                ec.uw_read(KBD_BL_DEFAULT_GREEN)?,
                ec.uw_read(KBD_BL_DEFAULT_BLUE)?,
            );
            ec.uw_write(KBD_BL_RGB_RED, dr)?;
            ec.uw_write(KBD_BL_RGB_GREEN, dg)?;
            ec.uw_write(KBD_BL_RGB_BLUE, db)?;
            kb_mode_rmw(ec, KBD_BL_RGB_MODE_APPLY, 0)?;
            let back = (
                ec.uw_read(KBD_BL_RGB_RED)?,
                ec.uw_read(KBD_BL_RGB_GREEN)?,
                ec.uw_read(KBD_BL_RGB_BLUE)?,
            );
            if back != (dr, dg, db) {
                return Err(format!(
                    "回读校验失败: 0x{:02X}/0x{:02X}/0x{:02X} != 0x{dr:02X}/0x{dg:02X}/0x{db:02X}",
                    back.0, back.1, back.2
                ));
            }
            Ok(J::o(vec![
                ("reset", J::B(true)),
                ("color", J::o(vec![
                    ("r", J::n(dr as f64)),
                    ("g", J::n(dg as f64)),
                    ("b", J::n(db as f64)),
                ])),
            ]))
        }
        other => Err(format!("未知 kb 动作: {other}")),
    }
}

fn api_ec_read(ec: &mut EcBackend, addr: &str, count: u16) -> Result<J, String> {
    let a = parse_hex_u16(addr).map_err(|_| format!("无效地址: {addr}"))?;
    let mut vals = Vec::new();
    for k in 0..count {
        let v = ec.uw_read(a + k)?;
        vals.push(J::o(vec![("addr", J::s(&format!("{:04X}", a + k))), ("value", J::n(v as f64))]));
    }
    Ok(J::o(vec![("addr", J::s(addr)), ("values", J::A(vals))]))
}

// ---------------------------------------------------------------- serve 守护模式（前端接口）

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

const EC_WRITE_SUPPORT_PATH: &str = "/sys/module/ec_sys/parameters/write_support";
static SERVE_STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn serve_stop_signal(_: c_int) {
    // 信号处理器只设置标志；sysfs 写入与等待请求结束均由主线程执行。
    SERVE_STOP.store(true, Ordering::Relaxed);
}

struct ServeSignals(Vec<(c_int, usize)>);

impl ServeSignals {
    fn install() -> Result<Self, String> {
        SERVE_STOP.store(false, Ordering::Relaxed);
        let mut guard = Self(Vec::new());
        for sig in [2, 15, 1] { // SIGINT、SIGTERM、SIGHUP（Linux）
            let old = unsafe { signal(sig, serve_stop_signal as *const () as usize) };
            if old == usize::MAX {
                return Err(format!("安装退出信号 {sig} 失败: {}", std::io::Error::last_os_error()));
            }
            guard.0.push((sig, old));
        }
        Ok(guard)
    }
}

impl Drop for ServeSignals {
    fn drop(&mut self) {
        for &(sig, old) in &self.0 {
            unsafe { signal(sig, old); }
        }
    }
}

/// 写支持是内核全局开关，服务退出时始终关闭（包括原先已开启的情况）。
struct EcSysWriteGuard {
    parameter: PathBuf,
    active: bool,
}

impl EcSysWriteGuard {
    fn prepare(parameter: &Path, load: impl FnOnce() -> Result<(), String>) -> Result<Self, String> {
        match std::fs::read_to_string(parameter) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => load()?,
            Err(e) => return Err(format!("读取 {} 失败: {e}", parameter.display())),
        }
        // 从此处起所有错误都经过 Drop，避免加载成功但后续启动失败遗留写权限。
        let guard = Self { parameter: parameter.into(), active: true };
        guard.set_enabled(true)?;
        Ok(guard)
    }

    fn set_enabled(&self, enabled: bool) -> Result<(), String> {
        std::fs::write(&self.parameter, if enabled { b"Y\n" } else { b"N\n" })
            .map_err(|e| format!("设置 {} 失败: {e}（需要 root，内核须允许修改参数）", self.parameter.display()))?;
        let actual = std::fs::read_to_string(&self.parameter)
            .map_err(|e| format!("回读 {} 失败: {e}", self.parameter.display()))?;
        if actual.trim() != if enabled { "Y" } else { "N" } {
            return Err(format!("{} 回读校验失败: {actual:?}", self.parameter.display()));
        }
        Ok(())
    }

    fn disable(&mut self) -> Result<(), String> {
        self.set_enabled(false)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for EcSysWriteGuard {
    fn drop(&mut self) {
        if self.active {
            if let Err(e) = self.disable() {
                eprintln!("警告: 关闭 ec_sys 写支持失败: {e}\n请手动执行: sudo sh -c 'echo N > {EC_WRITE_SUPPORT_PATH}'");
            }
        }
    }
}

struct ServeSocket(PathBuf);

impl Drop for ServeSocket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct ListenState {
    /// 开启监听的连接 id 集合。
    ///
    /// 按连接持有（而不是一个全局 bool）有两个好处：
    /// 1. 最后一个前端进程退出（含被 Ctrl+C 杀掉）时自动退出监听，不会留下守护进程
    ///    继续抢占模式键——这是全局 bool + 前端主动 disable 做不到的；
    /// 2. 多前端同时连接时，一方退出不会影响另一方。
    owners: std::collections::HashSet<u64>,
    current: Mode,
    last_bits: u8,
}

impl ListenState {
    fn enabled(&self) -> bool {
        !self.owners.is_empty()
    }
}

fn serve_broadcast(clients: &Mutex<Vec<(u64, UnixStream)>>, ev: &J) {
    let mut line = ev.dump();
    line.push('\n');
    let mut cs = clients.lock().unwrap();
    cs.retain_mut(|(_, s)| s.write_all(line.as_bytes()).is_ok());
}

/// 每条请求独立校验；成功一次不会免除后续请求的验证。
fn serve_authorized(req: &J, expected: Option<&str>) -> bool {
    let Some(expected) = expected else { return true; };
    let Some(actual) = req.get("token").and_then(J::as_str) else { return false; };
    // 避免按首个不同字节提前返回；长度不是秘密。
    if actual.len() != expected.len() {
        return false;
    }
    actual.bytes().zip(expected.bytes()).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

fn serve_handle(
    req: &J,
    ec: &Arc<Mutex<EcBackend>>,
    ls: &Arc<Mutex<ListenState>>,
    args: &Args,
    conn: u64,
) -> J {
    let id = req.get("id").cloned().unwrap_or(J::Null);
    let ok_resp = |data: J| J::o(vec![("id", id.clone()), ("ok", J::B(true)), ("data", data)]);
    let err_resp = |msg: &str| J::o(vec![("id", id.clone()), ("ok", J::B(false)), ("error", J::s(msg))]);

    if !serve_authorized(req, args.auth_token.as_deref()) {
        return err_resp("认证失败：缺少或无效的 token");
    }
    let Some(cmd) = req.get("cmd").and_then(|v| v.as_str()) else {
        return err_resp("缺少 cmd 字段");
    };
    let arg = |k: &str| req.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());

    let result: Result<J, String> = match cmd {
        "help" => Ok(J::o(vec![(
            "commands",
            J::A(vec![
                J::o(vec![("cmd", J::s("status")), ("desc", J::s("查询性能模式"))]),
                J::o(vec![("cmd", J::s("mode")), ("args", J::o(vec![("value", J::s("1|2|3|4|office|game|turbo|custom"))]))]),
                J::o(vec![("cmd", J::s("gpu")), ("args", J::o(vec![("action", J::s("status|hybrid|dgpu"))]))]),
                J::o(vec![("cmd", J::s("kb")), ("args", J::o(vec![("action", J::s("status|color|rainbow|brightness|on|off|reset")), ("value", J::s("按 action 而定"))]))]),
                J::o(vec![("cmd", J::s("ec-read")), ("args", J::o(vec![("addr", J::s("0x751")), ("count", J::s("默认 1"))]))]),
                J::o(vec![("cmd", J::s("listen")), ("args", J::o(vec![("enable", J::B(true))])), ("desc", J::s("启用后推送 {\"event\":\"mode-changed\",...}"))]),
            ]),
        )])),
        "status" => {
            let mut ecg = ec.lock().unwrap();
            api_status_ec(&mut ecg)
        }
        "mode" => {
            let Some(v) = arg("value") else {
                return err_resp("mode 需要 value 参数");
            };
            let mut ecg = ec.lock().unwrap();
            let r = api_mode_set_ec(&mut ecg, &v);
            // 本工具自己的写入不能被监听线程当成「按键」——否则刚点完「狂暴」，
            // 监听线程会把模式再推进一档。写完把寄存器值同步为监听基线。
            let bits = if r.is_ok() { ecg.uw_read(args.mode_reg).ok() } else { None };
            drop(ecg);
            if let Some(bits) = bits {
                if let Ok(mut lsg) = ls.lock() {
                    lsg.last_bits = bits;
                    if let Some(m) = mode_from_bits(bits) {
                        lsg.current = m;
                    }
                }
            }
            r
        }
        "gpu" => {
            let action = arg("action").unwrap_or_else(|| "status".into());
            api_gpu(args, &action, false)
        }
        "kb" => {
            let action = arg("action").unwrap_or_else(|| "status".into());
            let mut ecg = ec.lock().unwrap();
            api_kb(&mut ecg, &action, arg("value").as_deref(), false)
        }
        "ec-read" => {
            let addr = arg("addr").unwrap_or_else(|| "0x751".into());
            let count = req.get("count").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(1u64);
            let mut ecg = ec.lock().unwrap();
            api_ec_read(&mut ecg, &addr, count as u16)
        }
        "listen" => {
            let enable = req.get("enable").and_then(|v| v.as_bool()).unwrap_or(false);
            let mut lsg = ls.lock().unwrap();
            if enable {
                // 第一个 owner 负责初始化基线；后来者直接加入，不打断已在进行的循环
                if lsg.owners.is_empty() {
                    let mut ecg = ec.lock().unwrap();
                    lsg.current = ecg.read_state().ok().and_then(|(m, _)| m).unwrap_or(Mode::Office);
                    lsg.last_bits = ecg.uw_read(args.mode_reg).unwrap_or(0);
                }
                lsg.owners.insert(conn);
            } else {
                lsg.owners.remove(&conn);
            }
            let listening = lsg.enabled();
            let owners = lsg.owners.len() as f64;
            Ok(J::o(vec![("listening", J::B(listening)), ("owners", J::n(owners))]))
        }
        other => Err(format!("未知命令: {other}（help 查看命令列表）")),
    };

    match result {
        Ok(data) => ok_resp(data),
        Err(e) => err_resp(&e),
    }
}

fn cmd_serve(args: &Args) {
    if let Err(e) = run_serve(args) {
        // run_serve 的资源已析构；不可在持有写权限 guard 时调用 process::exit。
        die(&e);
    }
}

fn run_serve(args: &Args) -> Result<(), String> {
    if args.backend == BackendSel::PlatformProfile {
        return Err("serve 仅支持 ec-direct 后端".into());
    }
    let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open("/run/firebat-cc-ec.lock")
        .map_err(|e| format!("打开服务锁失败: {e}（请以 root 运行）"))?;
    if unsafe { flock(lock.as_raw_fd(), 2 | 4) } != 0 { // LOCK_EX | LOCK_NB
        return Err(format!("无法独占 ec_sys 服务锁，可能已有 serve 正在运行: {}", std::io::Error::last_os_error()));
    }
    // 锁文件不可 unlink：保持 inode 稳定，文件描述符关闭自动解锁。
    let _signals = ServeSignals::install()?;
    if args.exit_on_stdin_close {
        std::thread::spawn(|| {
            wait_for_input_close(std::io::stdin().lock());
            SERVE_STOP.store(true, Ordering::Release);
        });
    }
    let mut ec_write = EcSysWriteGuard::prepare(Path::new(EC_WRITE_SUPPORT_PATH), || {
        let output = std::process::Command::new("modprobe")
            .args(["ec_sys", "write_support=1"]).output()
            .map_err(|e| format!("执行 modprobe ec_sys write_support=1 失败: {e}"))?;
        if !output.status.success() {
            return Err(format!("加载 ec_sys 失败（{}）: {}", output.status, String::from_utf8_lossy(&output.stderr).trim()));
        }
        Ok(())
    })?;
    let socket_path = PathBuf::from(args.positional.get(1).cloned().unwrap_or_else(|| "/run/firebat-cc.sock".into()));
    let ec = Arc::new(Mutex::new(EcBackend::open(args.mode_reg)
        .map_err(|e| format!("{e}\n若 debugfs 尚未挂载，请执行: sudo mount -t debugfs debugfs /sys/kernel/debug"))?));

    let _ = std::fs::remove_file(&socket_path);
    if let Some(parent) = socket_path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    let listener = UnixListener::bind(&socket_path)
        .map_err(|e| format!("绑定 {} 失败: {e}", socket_path.display()))?;
    let _socket = ServeSocket(socket_path.clone());
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o666))
        .map_err(|e| format!("设置 socket 权限失败: {e}"))?;
    listener.set_nonblocking(true).map_err(|e| format!("设置非阻塞监听失败: {e}"))?;
    // 请求与轮询持读锁；退出持写锁，等待硬件操作结束后才关闭写支持。
    let operations = Arc::new(std::sync::RwLock::new(()));

    let clients: Arc<Mutex<Vec<(u64, UnixStream)>>> = Arc::new(Mutex::new(Vec::new()));
    let ls: Arc<Mutex<ListenState>> = Arc::new(Mutex::new(ListenState {
        owners: std::collections::HashSet::new(),
        current: Mode::Office,
        last_bits: 0,
    }));

    println!("firebat-cc serve 已启动: {}", socket_path.display());
    println!("协议: 每行一个 JSON 请求，如 {{\"id\":1,\"cmd\":\"status\"}}；先发 help 查看命令");
    if args.auth_token.is_some() {
        println!("访问验证已开启：每条请求必须携带 token 字段；验证前不推送模式事件。");
    }
    println!("监听事件推送形如 {{\"event\":\"mode-changed\",\"from\":...,\"to\":...}}");
    println!("ec_sys 写支持已开启；Ctrl+C / SIGTERM 退出时关闭写支持。");

    // 模式监听轮询线程
    {
        let ec2 = ec.clone();
        let clients2 = clients.clone();
        let ls2 = ls.clone();
        let mode_reg = args.mode_reg;
        let interval = args.interval;
        let operations2 = operations.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(interval));
            let _operation = operations2.read().unwrap();
            if SERVE_STOP.load(Ordering::Relaxed) {
                break;
            }
            let enabled = ls2.lock().map(|l| l.enabled()).unwrap_or(false);
            if !enabled {
                continue;
            }
            let ev = (|| -> Option<J> {
                let mut lsg = ls2.lock().unwrap();
                if !lsg.enabled() {
                    return None;
                }
                let mut ecg = ec2.lock().unwrap();
                let bits = ecg.uw_read(mode_reg).ok()?;
                if bits == lsg.last_bits {
                    return None;
                }
                let from = lsg.current;
                let target = next_mode(from);
                ecg.write_mode(target).ok()?;
                lsg.current = target;
                lsg.last_bits = ecg.uw_read(mode_reg).unwrap_or(bits);
                Some(J::o(vec![
                    ("event", J::s("mode-changed")),
                    (
                        "from",
                        J::o(vec![("index", J::n(from.index() as f64)), ("name", J::s(from.zh()))]),
                    ),
                    (
                        "to",
                        J::o(vec![("index", J::n(target.index() as f64)), ("name", J::s(target.zh()))]),
                    ),
                ]))
            })();
            drop(_operation);
            if let Some(ev) = ev {
                serve_broadcast(&clients2, &ev);
            }
        });
    }

    // 接受连接
    let mut next_conn: u64 = 1;
    let mut result = Ok(());
    while !SERVE_STOP.load(Ordering::Relaxed) {
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                result = Err(format!("接受连接失败: {e}"));
                break;
            }
        };
        let conn_id = next_conn;
        next_conn += 1;
        let clients2 = clients.clone();
        if args.auth_token.is_none() {
            if let Ok(clone) = stream.try_clone() {
                clients2.lock().unwrap().push((conn_id, clone));
            }
        }
        let ec2 = ec.clone();
        let ls2 = ls.clone();
        let args2 = args.clone();
        let operations2 = operations.clone();
        std::thread::spawn(move || {
            let _ = stream.write_all(b"{\"event\":\"hello\",\"version\":\"0.6.0\"}\n");
            let mut reader = std::io::BufReader::new(match stream.try_clone() {
                Ok(r) => r,
                Err(_) => return,
            });
            let mut registered = args2.auth_token.is_none();
            let mut line = String::new();
            loop {
                line.clear();
                match std::io::BufRead::read_line(&mut reader, &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if line.trim().is_empty() {
                    continue;
                }
                let operation = operations2.read().unwrap();
                if SERVE_STOP.load(Ordering::Relaxed) {
                    break;
                }
                let resp = match jparse(&line) {
                    Ok(req) => {
                        if !registered && serve_authorized(&req, args2.auth_token.as_deref()) {
                            if let Ok(clone) = stream.try_clone() {
                                clients2.lock().unwrap().push((conn_id, clone));
                                registered = true;
                            }
                        }
                        serve_handle(&req, &ec2, &ls2, &args2, conn_id)
                    }
                    Err(e) => J::o(vec![
                        ("id", J::Null),
                        ("ok", J::B(false)),
                        ("error", J::s(&format!("JSON 解析失败: {e}"))),
                    ]),
                };
                drop(operation);
                let mut out = resp.dump();
                out.push('\n');
                if stream.write_all(out.as_bytes()).is_err() {
                    break;
                }
            }
            // 连接断开：把它从监听 owner 里摘掉。前端崩溃 / 被 Ctrl+C 时不会走到
            // listen enable:false，这里兜底，避免守护进程继续抢占模式键。
            if let Ok(mut lsg) = ls2.lock() {
                lsg.owners.remove(&conn_id);
            }
            clients2.lock().unwrap().retain(|(id, _)| *id != conn_id);
        });
    }
    SERVE_STOP.store(true, Ordering::Relaxed);
    let _operation = operations.write().unwrap();
    ec_write.disable()?;
    println!("firebat-cc serve 已关闭，ec_sys 写支持已关闭。");
    result
}

fn wait_for_input_close(mut input: impl Read) {
    let mut buffer = [0u8; 128];
    loop {
        match input.read(&mut buffer) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gui_lifetime_arguments_and_input_eof() {
        assert!(test_args(&["serve", "--exit-on-stdin-close"]).unwrap().exit_on_stdin_close);
        assert!(!test_args(&["serve"]).unwrap().exit_on_stdin_close);
        assert!(test_args(&["status", "--exit-on-stdin-close"]).is_err());
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || { wait_for_input_close(reader); tx.send(()).unwrap(); });
        writer.write_all(b"still alive").unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(writer);
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    fn test_args(values: &[&str]) -> Result<Args, String> {
        parse_argv(values.iter().map(|v| (*v).to_string()).collect())
    }

    #[test]
    fn serve_token_arguments() {
        assert!(test_args(&["serve"]).unwrap().auth_token.is_none());
        let args = test_args(&["serve", "/tmp/test.sock", "--auth-token", "秘密 token"]).unwrap();
        assert_eq!(args.auth_token.as_deref(), Some("秘密 token"));
        assert_eq!(args.positional, ["serve", "/tmp/test.sock"]);
        assert!(test_args(&["--auth-token", "abc", "serve"]).is_ok());
        for argv in [
            vec!["serve", "--auth-token"],
            vec!["serve", "--auth-token", ""],
            vec!["serve", "--auth-token", "--json"],
            vec!["status", "--auth-token", "abc"],
        ] {
            assert!(test_args(&argv).is_err());
        }
    }

    #[test]
    fn serve_token_validation() {
        for input in [r#"{}"#, r#"{"token":null}"#, r#"{"token":123}"#,
            r#"{"token":true}"#, r#"{"token":[]}"#, r#"{"token":{}}"#,
            r#"{"token":""}"#, r#"{"token":"abD"}"#, r#"{"token":"abc "}"#] {
            let req = jparse(input).unwrap();
            assert!(!serve_authorized(&req, Some("abc")), "{input}");
            assert!(serve_authorized(&req, None));
        }
        for token in ["abc", "秘密 token", "a\"b\\c\nd"] {
            let req = jparse(&J::o(vec![("token", J::s(token))]).dump()).unwrap();
            assert!(serve_authorized(&req, Some(token)));
        }
    }

    #[test]
    fn serve_auth_precedes_all_commands_and_checks_every_request() {
        // /dev/null 代替 EC 文件；测试不访问硬件。
        let ec = Arc::new(Mutex::new(EcBackend {
            io: OpenOptions::new().read(true).write(true).open("/dev/null").unwrap(),
            mode_reg: MODE_REG_DEFAULT,
        }));
        let ls = Arc::new(Mutex::new(ListenState {
            owners: [7].into_iter().collect(), current: Mode::Office, last_bits: 0xA0,
        }));
        let args = test_args(&["serve", "--auth-token", "secret"]).unwrap();
        let help = jparse(r#"{"id":1,"cmd":"help","token":"secret"}"#).unwrap();
        assert_eq!(serve_handle(&help, &ec, &ls, &args, 7).get("ok"), Some(&J::B(true)));
        for cmd in ["help", "status", "mode", "gpu", "kb", "ec-read", "listen", "unknown"] {
            for token in [None, Some("wrong")] {
                let mut fields = vec![("id", J::s("req")), ("cmd", J::s(cmd)), ("enable", J::B(false))];
                if let Some(token) = token { fields.push(("token", J::s(token))); }
                let resp = serve_handle(&J::o(fields), &ec, &ls, &args, 7);
                assert_eq!(resp.get("id"), Some(&J::s("req")));
                assert_eq!(resp.get("ok"), Some(&J::B(false)));
                assert_eq!(resp.get("error").and_then(J::as_str), Some("认证失败：缺少或无效的 token"));
                assert!(!resp.dump().contains("secret"));
            }
        }
        assert!(ls.lock().unwrap().owners.contains(&7));
        assert_eq!(serve_handle(&help, &ec, &ls, &args, 7).get("ok"), Some(&J::B(true)));
        let no_auth = test_args(&["serve"]).unwrap();
        let help = jparse(r#"{"cmd":"help"}"#).unwrap();
        assert_eq!(serve_handle(&help, &ec, &ls, &no_auth, 7).get("ok"), Some(&J::B(true)));
    }

    #[test]
    fn ec_write_lifecycle() {
        // 普通临时文件模拟模块参数，不加载模块或访问 EC。
        let dir = std::env::temp_dir().join(format!("firebat-ec-lifecycle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let parameter = dir.join("write_support");
        for initial in ["N\n", "Y\n"] {
            std::fs::write(&parameter, initial).unwrap();
            let guard = EcSysWriteGuard::prepare(&parameter, || panic!("已加载时不应 modprobe")).unwrap();
            assert_eq!(std::fs::read_to_string(&parameter).unwrap(), "Y\n");
            drop(guard); // 同时覆盖后续启动失败时的回滚路径
            assert_eq!(std::fs::read_to_string(&parameter).unwrap(), "N\n");
        }
        std::fs::remove_file(&parameter).unwrap();
        let mut guard = EcSysWriteGuard::prepare(&parameter, || {
            std::fs::write(&parameter, "Y\n").map_err(|e| e.to_string())
        }).unwrap();
        guard.disable().unwrap();
        assert_eq!(std::fs::read_to_string(&parameter).unwrap(), "N\n");
        drop(guard);
        std::fs::remove_file(&parameter).unwrap();
        assert!(EcSysWriteGuard::prepare(&parameter, || Err("modprobe failed".into())).is_err());
        assert!(!parameter.exists());
        std::fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn json_roundtrip() {
        let src = r#"{"id":1,"cmd":"kb","args":{"value":"ff8000","on":true,"n":null,"arr":[1,2.5,-3]}}"#;
        let v = jparse(src).unwrap();
        assert_eq!(v.get("cmd").and_then(|j| j.as_str()), Some("kb"));
        assert_eq!(v.get("args").unwrap().get("on").unwrap().as_bool(), Some(true));
        let dump = v.dump();
        let v2 = jparse(&dump).unwrap();
        assert_eq!(v, v2);
    }

    #[test]
    fn json_truncated_unicode_does_not_panic_before_auth() {
        for suffix in ["", "1", "12", "123"] {
            assert!(jparse(&format!(r#"{{"token":"\u{suffix}"#)).is_err());
        }
    }

    #[test]
    fn json_escapes() {
        let v = J::s("a\"b\\c\nd\te");
        let d = v.dump();
        assert_eq!(jparse(&d).unwrap(), v);
    }
}
