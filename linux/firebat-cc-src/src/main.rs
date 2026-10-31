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
use std::time::Duration;

// efivarfs 文件默认带 immutable 标志（写前需清除，写后恢复）
extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
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

struct Args {
    backend: BackendSel,
    mode_reg: u16,
    dry_run: bool,
    mux_offset: u16,
    hybrid_value: u8,
    dgpu_value: u8,
    positional: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut backend = BackendSel::Auto;
    let mut mode_reg = MODE_REG_DEFAULT;
    let mut dry_run = false;
    let mut mux_offset = MUX_OFFSET_DEFAULT;
    let mut hybrid_value = MUX_HYBRID_DEFAULT;
    let mut dgpu_value = MUX_DGPU_DEFAULT;
    let mut positional: Vec<String> = Vec::new();
    let argv: Vec<String> = std::env::args().skip(1).collect();

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
            "--dry-run" => dry_run = true,
            "-h" | "--help" | "help" => {
                print_help();
                std::process::exit(0);
            }
            "-v" | "--version" => {
                println!("firebat-cc 0.2.2");
                std::process::exit(0);
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }

    Ok(Args {
        backend,
        mode_reg,
        dry_run,
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
        "firebat-cc 0.2.2 — Firebat GX 性能模式/显卡输出控制工具（Linux 逆向移植版）

用法:
  firebat-cc status                        查看当前性能模式
  firebat-cc mode <MODE>                   切换性能模式
  firebat-cc ec-read <ADDR> [COUNT]        读取 EC 寄存器（调试用，16 进制地址）
  firebat-cc gpu status                    查看显卡输出模式（MUX）
  firebat-cc gpu hybrid                    切换为混合输出（重启生效）
  firebat-cc gpu dgpu                      切换为独显直连（重启生效）
  firebat-cc gpu raw <0-255>               直接写 MUX 字节（调试用，高危）
  firebat-cc uefi-dump                     十六进制转储 UEFI 厂商变量

MODE 取值:
  1 | office | 办公        2 | game | balance | 游戏(均衡)
  3 | turbo | 狂暴         4 | custom | 自定义（v0.1 等同于 1 档，接口保留）

选项:
  --backend <auto|ec-direct|platform-profile>   性能模式后端选择（默认 auto）
  --mode-reg <hex>                              模式寄存器地址覆盖（默认 0x0751）
  --mux-offset <hex|dec>                        MUX 字段偏移覆盖（默认 98 = 0x62）
  --hybrid-value <n>                            混合输出对应值覆盖（默认 0）
  --dgpu-value <n>                              独显直连对应值覆盖（默认 1）
  --dry-run                                     只显示将要执行的操作，不实际写入
  -h, --help                                    显示帮助
  -v, --version                                 显示版本

示例:
  sudo modprobe ec_sys write_support=1     # ec-direct 性能模式后端需要
  sudo firebat-cc status
  sudo firebat-cc mode turbo
  sudo firebat-cc gpu dgpu                 # 独显直连，重启生效
  sudo firebat-cc uefi-dump                # 两种输出模式下各转储一次，diff 确认 MUX 偏移与取值

原理简述:
  性能模式写入 EC 0x0751 的模式位（办公 0xA0 / 均衡 0x20 / 狂暴 0x30）。
  显卡输出写入 UEFI 厂商变量 UniWillVariable/OemMagicVariable（GUID
  {{9f33f85c-13ca-4fd1-9c4a-96217722c593}}）中的 OemDisplayMode 字段，
  与 Windows 端控制中心同为「读整块-改一字段-写整块」，BIOS 重启时应用。

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
        "ec-read" => cmd_ec_read(&args),
        "gpu" => cmd_gpu(&args),
        "uefi-dump" => cmd_uefi_dump(&args),
        _ => {
            eprintln!("未知命令: {cmd}\n（使用 -h 查看帮助）");
            std::process::exit(2);
        }
    }
}

fn cmd_status(args: &Args) {
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

    let mut uefi = match UefiBackend::open(args.mux_offset, args.hybrid_value, args.dgpu_value) {
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
