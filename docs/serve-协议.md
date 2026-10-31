# firebat-cc serve 协议（前端开发接口）

v0.6.0 · 零依赖 JSON 行协议，适用于 Tauri / GTK / Electron / Python 等任何前端。

## 启动

```bash
sudo modprobe ec_sys write_support=1          # 每次开机一次
sudo ./target/release/firebat-cc serve        # 默认 /run/firebat-cc.sock
sudo ./target/release/firebat-cc serve /tmp/fb.sock   # 自定义路径
```

守护进程以 root 运行（持有 EC 权限），socket 权限 0666，**前端进程无需 root**。

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
| `listen` | `enable`：**JSON bool**（不是字符串） | `{listening, owners}`；启用后守护进程轮询 EC 0x0751，检测到按键即按 办公(1)→游戏(2)→狂暴(3) 顺序推进档位，并向所有连接广播 `mode-changed` 事件 |

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
  连接，不只是 owner；未开启监听时不会有事件。
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
- socket 路径下任意本地用户可控制硬件，多用户环境注意（未来可加 0660 + 用户组）。
