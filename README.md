# Firebat GX 控制中心（Linux 移植）

对火影（Firebat）GX 随机附带的 Windows UWP「控制中心」做逆向，并在 Linux 上重建
等效能力：性能模式切换、显卡输出（MUX）切换、键盘背光与 EC 调试。

本项目按**纯 Linux 项目**组织——可构建、可运行的代码全部位于仓库主目录（Cargo
工作区），Windows 侧安装包与解包产物仅作为逆向素材隔离存放在 `reference/windows/`。

## 组成

| 组件 | 说明 |
|---|---|
| `firebat-cc`（根包，`src/`） | 命令行守护/工具，直接操作 EC 与 UEFI 变量；Rust，零第三方依赖，单文件二进制 |
| `firebat-cc-gui`（`gui/`） | Slint 图形前端，经 Unix socket 与 `firebat-cc serve` 通信，**不需要 root** |

## 目录结构

```
.
├── Cargo.toml            # 工作区定义 + CLI 包（统一 [profile.release]）
├── Cargo.lock            # 工作区单一锁文件
├── src/main.rs           # firebat-cc CLI 源码
├── gui/                  # firebat-cc-gui 图形前端（Slint 成员包）
│   ├── ui/*.slint        # 界面与主题（编译期嵌入）
│   ├── src/              # 回调接线 + serve 协议客户端
│   ├── examples/         # 离线截图渲染
│   └── tools/            # mock-daemon.py（无真机联调）
├── docs/                 # serve 协议、专项调查记录
├── reference/windows/    # 逆向素材：安装包、解包产物、UWP 包（不参与构建、大文件不入库）
├── 分析报告.md           # 逆向总报告（唯一事实来源）
└── AGENTS.md             # 协作规则（回写报告、真机验证、提交纪律）
```

## 构建

```bash
# Arch
sudo pacman -S --needed base-devel pkgconf libxkbcommon wayland fontconfig freetype2
# Debian/Ubuntu
sudo apt install build-essential pkg-config libxkbcommon-dev libfontconfig1-dev libfreetype6-dev

cargo build --release          # 构建 CLI + GUI
cargo build --release -p firebat-cc       # 仅 CLI
cargo build --release -p firebat-cc-gui   # 仅 GUI
```

产物统一在 `target/release/`：`firebat-cc`、`firebat-cc-gui`（均为单文件，可复制到
任意位置运行）。

## 运行

```bash
sudo modprobe ec_sys write_support=1        # 每次开机一次，EC 直写需要
sudo ./target/release/firebat-cc serve      # 守护进程，默认 /run/firebat-cc.sock
./target/release/firebat-cc-gui             # 图形界面（普通用户）
```

也可以不走守护进程，直接单次调用：

```bash
sudo ./target/release/firebat-cc status
sudo ./target/release/firebat-cc mode turbo
sudo ./target/release/firebat-cc kb status
```

完整命令见 `./target/release/firebat-cc --help`，前端接口见
[`docs/serve-协议.md`](docs/serve-协议.md)。

## 文档

- [`分析报告.md`](分析报告.md) —— 逆向总报告（EC 寄存器、UEFI 变量、调用链，唯一事实来源）
- [`docs/serve-协议.md`](docs/serve-协议.md) —— 守护进程 JSON 行协议（前端开发接口）
- [`docs/逆向记录-自定义模式指示灯.md`](docs/逆向记录-自定义模式指示灯.md) —— 专项调查归档
- [`AGENTS.md`](AGENTS.md) —— 协作与真机验证规则

## 逆向素材

`reference/windows/` 保留原始安装包、`extracted/` 解包产物、`PreinstallKit/*.msixbundle`
（UWP 包，含托盘程序与语言资源）等，供后续继续逆向使用。这些文件体积大（安装包约
177 MB）已在 `.gitignore` 中排除，仅小型可读配置（`setup.ini`、`RGBKeyboard.reg`、
安装脚本）入库。Linux 侧代码与构建**不得**依赖该目录。

## 安全提示

直接写 EC、写 UEFI 变量属于底层硬件操作。工具对模式寄存器只做「读-改-写（仅触碰
目标位）+ 回读校验」，但仍请在了解风险的前提下于真机测试；0/1 与混合/独显的对应
关系为逆向推断，可用 `--hybrid-value` / `--dgpu-value` 覆盖。
