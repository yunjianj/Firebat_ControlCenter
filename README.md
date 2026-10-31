# Firebat GX 控制中心（Linux 移植）

对火影（Firebat）GX 随机附带的 Windows UWP「控制中心」做逆向，并在 Linux 上重建
等效能力：性能模式切换、显卡输出（MUX）切换、键盘背光与 EC 调试。

**本软件通过逆向分析 Windows 版控制中心获取相关接口，并通过 AI 进行开发。**
这是社区 Linux 移植项目，非厂商官方软件。

## 适配机型与实测范围

**目前仅在火影 Huan 16 Pro 上进行过真机实测，当前适配目标为该机型。**
接口依据随机器提供的 Firebat GX Windows 控制中心（5.49.51.44）逆向分析实现。
其他火影机型、其他品牌的同模具机型，以及不同 BIOS / EC 固件版本，尚未验证兼容性；
安装包中包含其他机型配置不代表本软件已经适配这些机型。

已实测过该机型不代表全部功能或后续改动均已在真机验证。各接口与功能的具体验证
状态以 [`分析报告.md`](分析报告.md) 的逐项记录为准，静态推断项仍需只读检查和差分验证。

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
./target/release/firebat-cc-gui             # 后端未运行时自动启动同目录 firebat-cc serve
```

GUI 先连接已有后端；未运行时通过 `pkexec` 请求系统授权并自动启动同目录后端，
需要安装 polkit 并运行桌面认证代理。GUI 保持普通用户权限；自定义 `--socket` /
`FIREBAT_CC_SOCK` 同样传给自动启动的后端。关闭窗口会进入托盘并保留模式键监听；
左键托盘恢复窗口，右键菜单可「打开」或「关闭」。GUI 自动启动的后端使用随机凭证；
从菜单关闭 GUI 后，会同步关闭该后端并清理 EC 写支持。手动启动的已有后端保持独立。
桌面不支持托盘时，关闭窗口直接退出 GUI。
挡位切换时会在左上角显示半透明黑底白字提示，5 秒后消失；托盘运行时同样生效。
也可手动启动：`sudo ./target/release/firebat-cc serve`。
可附加 `--auth-token '随机长凭证'`，此后每条 socket JSON 请求必须带相同的
`"token"` 字段；不带参数保持兼容。GUI 连接手动启用认证的服务时可使用
`--auth-token TOKEN` 或 `FIREBAT_CC_TOKEN` 环境变量，详见[访问验证说明](docs/serve-协议.md#可选访问凭证)。

`serve` 启动失败或收到 Ctrl+C / SIGTERM / SIGHUP 时关闭 `ec_sys` 写支持并回读
校验（即使启动前已开启），保留模块。仅允许一个服务实例，包括使用不同 socket 路径
的实例。SIGKILL、进程崩溃和断电无法执行清理；必要时手动关闭：
`sudo sh -c 'echo N > /sys/module/ec_sys/parameters/write_support'`。
若提示 debugfs 未挂载，先执行 `sudo mount -t debugfs debugfs /sys/kernel/debug`。

也可以不走守护进程，直接单次调用：

```bash
sudo modprobe ec_sys write_support=1        # 单次命令仍需手动准备；已加载时用下行开启
sudo sh -c 'echo Y > /sys/module/ec_sys/parameters/write_support'
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
