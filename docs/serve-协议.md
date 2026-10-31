# firebat-cc serve 协议（前端开发接口）

v0.6.0 · 零依赖 JSON 行协议，适用于 Tauri / GTK / Electron / Python 等任何前端。

## 启动

GUI 启动时会先连接已有服务；若 socket 不存在或连接被拒绝，则通过 `pkexec`
请求系统授权，启动 GUI 可执行文件同目录的 `firebat-cc serve`，并传递所选 socket。
GUI 自动启动时传递随机凭证并绑定 stdin 管道，GUI 真正退出后服务同步正常退出。
关闭 GUI 窗口进入托盘仍保持服务运行。也可手动启动独立服务：

```bash
sudo ./target/release/firebat-cc serve        # 自动准备 ec_sys；默认 /run/firebat-cc.sock
sudo ./target/release/firebat-cc serve /tmp/fb.sock   # 自定义路径
```

守护进程以 root 运行（持有 EC 权限），socket 权限 0666，**前端进程无需 root**。

`serve` 在模块未加载时自动执行 `modprobe ec_sys write_support=1`；已加载时直接
开启 `/sys/module/ec_sys/parameters/write_support`，并回读校验。启动失败或
Ctrl+C / SIGTERM / SIGHUP 退出时，等待正在执行的硬件请求完成，再关闭写支持并
回读校验（启动前已开启也会关闭），移除 socket，保留模块。全局文件锁防止多个
`serve` 同时管理写支持，即使使用不同 socket 路径也只允许一个实例。

debugfs 需事先挂载：`sudo mount -t debugfs debugfs /sys/kernel/debug`。
SIGKILL、进程崩溃和断电无法执行清理；如需手动关闭写支持：
`sudo sh -c 'echo N > /sys/module/ec_sys/parameters/write_support'`。
此开关是全局参数，服务存续期间其他 root 程序也能使用 EC 写支持。

## 可选访问凭证

```bash
sudo ./target/release/firebat-cc serve --auth-token 'your-random-secret'
# 自定义 socket 也可使用：serve /tmp/fb.sock --auth-token 'your-random-secret'
```

启用后，**每条请求**都必须携带顶层字符串 `token`，包含 `help`、查询和
`listen`；此前验证成功不会免除后续请求的校验。例如：

```json
{"id":1,"cmd":"status","token":"your-random-secret"}
{"id":2,"cmd":"listen","enable":true,"token":"your-random-secret"}
```

缺失、非字符串或值不匹配时，返回
`{"id":1,"ok":false,"error":"认证失败：缺少或无效的 token"}`，不执行命令。
未验证的连接仅能接收 `hello`，不能收到 `mode-changed`；同一连接提供一次正确
凭证后可接收广播，但每条后续请求仍须携带凭证。断线重连需重新验证。

不传 `--auth-token` 保持兼容，不校验凭证；空凭证、以 `--` 开头的凭证、
缺少参数或用于非 `serve` 命令会报错。凭证按原字符串精确匹配，不会去除空白。服务不会打印或回显凭证。
使用随机长凭证；命令行参数可能被同机进程查看，这不构成同一用户/root 之间的隔离。
GUI 自动启动后端时使用 32 字节随机数编码的 64 位十六进制凭证，每条请求均携带。
连接手动启用认证的后端时，GUI 可通过 `--auth-token TOKEN` 或环境变量
`FIREBAT_CC_TOKEN` 提供凭证；命令行优先。

## GUI 生命周期绑定

`serve --exit-on-stdin-close` 在标准输入 EOF 或读取错误时正常退出；此前输入的数据
被丢弃，不是命令协议。GUI 自动启动后端时同时传入此选项并保持管道写端，真正退出
或崩溃后管道关闭，后端等待正在进行的硬件操作结束，再关闭 EC 写支持并回读、移除
socket。GUI 正常退出最多等待子进程 10 秒；超时会报告，管道仍保持关闭。
该参数仅适用于 `serve`，默认关闭，手动启动的服务行为不变。GUI 复用手动服务时
只释放自己的监听连接，不关闭独立服务。

## 传输

Unix domain socket，**每行一个 JSON**（`\n` 分隔）。

- 连接建立后服务端先推送 `{"event":"hello","version":"0.6.0"}`
- 请求：`{"id":1,"cmd":"...","args...}`（`id` 任意，用于配对响应）
- 响应：`{"id":1,"ok":true,"data":{...}}` 或 `{"id":1,"ok":false,"error":"..."}`
- 事件（无 id，随时推送）：`{"event":"mode-changed","from":{"index":1,"name":"办公"},"to":{...}}`

## 命令

| cmd | 参数 | 返回 data |
|---|---|---|
| `help` | — | 命令列表 |
| `status` | — | `{mode, name, bits, backend, detail}` |
| `mode` | `value`: `1\|2\|3\|4\|office\|game\|turbo\|custom` | `{mode, name, bits}` |
| `gpu` | `action`: `status\|hybrid\|dgpu` | status: `{var, mux_raw, interpreted, payload_len}`；切换: `{written, unchanged, reboot_required}` |
| `kb` | `action`: `status\|color\|rainbow\|brightness\|on\|off\|reset`；`value`: 颜色 `ff8000`/`red`、亮度 0-100、on/off | status: `{white_only, disabled, brightness_level, brightness_max, rainbow, color:{r,g,b}}` |
| `ec-read` | `addr`: `0x751`，`count`: 默认 1 | `{addr, values:[{addr,value},...]}` |
| `listen` | `enable`：**JSON bool**（不是字符串） | `{listening, owners}`；启用后守护进程轮询 EC 0x0751，检测到按键即按 办公(1)→游戏(2)→狂暴(3) 顺序推进档位，并向所有通过访问验证的连接（未启用验证时为所有连接）广播 `mode-changed` 事件 |

kb color 的 r/g/b 为 EC 通道值（0-50 值域）；kb brightness 为 EC 0-4 级。

kb reset 的语义与 CLI `firebat-cc kb reset` 一致：读取固件默认色 `0x076C-0x076E`，抄写到
颜色寄存器 `0x0769-0x076B` 后置 `0x0767` 的 APPLY_COLOR 位，返回 `{reset, color:{r,g,b}}`
（回读校验失败会返回错误）。注意 `0x078C` 的 `0x10` 与 APPLY_COLOR 都是一次性触发位，
EC 消费后自动清零——单置 `0x10` 只是「重新应用当前颜色」，不会恢复默认色。
重置不改亮度级与开关状态；彩虹循环开启时颜色会被循环覆盖，前端应提示先关闭彩虹。

### listen：监听模式（前端应常开）

EC 固件自行处理性能模式键（Fn 组合键），用户态拿不到任何输入事件，只能**轮询
0x0751 事后纠正**。因此前端连上守护进程后应立刻发一次 `listen {enable:true}`，
并保持到退出：

- **按连接持有**：`enable:true` 把当前连接的 id 加进 owner 集合，`false` 移除；
  `listening` 表示集合是否非空，`owners` 是当前持有者数量。多个前端互不影响。
- **断线自动摘除**：owner 连接断开（前端崩溃、被 `Ctrl+C`、图形界面直接关掉）时，
  守护进程自动把它从集合里去掉，最后一个 owner 断开即退出监听。
  所以前端不必依赖"退出时发 disable"，那句只是双保险。
- **自身写入同步基线**：通过 `mode` 命令改档后，守护进程同步 `last_bits` 与 `current`，
  不会把自己的写入误判成按键（否则点完「狂暴」会被再推进一档）。
- 事件 `{"event":"mode-changed","from":{"index","name"},"to":{...}}` 广播给**所有**
  连接（启用访问验证时仅限已验证的连接），不只是 owner；未开启监听时不会有事件。
- 监听期间建议前端保留兜底的 `status` 轮询（守护进程重启、事件丢失时可自愈）。
  事件只带 index/name，`bits`/`detail` 需再取一次 `status`。

## 测试方法

```bash
socat - UNIX-CONNECT:/run/firebat-cc.sock
> {"id":1,"cmd":"status"}
< {"id":1,"ok":true,"data":{"mode":1,...}}
```

## 单次调用模式（不走守护进程）

所有命令支持 `--json`，输出 `{"ok":true,"data":...}` / `{"ok":false,"error":...}`：

```bash
sudo ./target/release/firebat-cc status --json
sudo ./target/release/firebat-cc mode turbo --json
sudo ./target/release/firebat-cc kb status --json
```

`--dry-run` 与 `--json` 可叠加（返回 `dry_run: true`，不写硬件）。

## 官方前端

`gui/`（Cargo 成员包 `firebat-cc-gui`）是基于本协议的 Slint 图形界面（Rust，无需 root）：

```bash
sudo ./target/release/firebat-cc serve &
./target/release/firebat-cc-gui                         # 默认 /run/firebat-cc.sock
./target/release/firebat-cc-gui --socket /tmp/fb.sock
```

前端只做「协议 ↔ 界面状态」的映射，协议改动后同步修改 `gui/ui/state.slint` 与
`gui/src/main.rs` 即可。没有真机时可用 `gui/tools/mock-daemon.py` 联调
（纯内存模拟，支持上面全部命令）。

## 注意

- `mode`/`gpu` 切换、`kb` 写入是真实硬件操作，前端应提供确认交互。
- `mode custom` 在 v0.6.0 等同办公档（自定义配置接口留待后续）。
- 未启用 `--auth-token` 时，socket 路径下任意本地用户可控制硬件。
