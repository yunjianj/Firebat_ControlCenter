#!/usr/bin/env python3
"""firebat-cc serve 的模拟实现（仅用于前端联调，不碰硬件）。

    python3 tools/mock-daemon.py /run/firebat-cc.sock

状态保存在内存中，支持协议里的全部命令：
status / mode / gpu / kb / ec-read / listen / help
"""

import json
import os
import socket
import socketserver
import sys
import threading
import time

MODE_BITS = {1: 0xA0, 2: 0x20, 3: 0x30, 4: 0xA0}
MODE_NAMES = {1: "办公", 2: "游戏(均衡)", 3: "狂暴", 4: "自定义"}
# 工具定义的循环顺序（与 EC 固件自身的 1→3→2 不同）
MODE_CYCLE = {1: 2, 2: 3, 3: 1, 4: 1}

STATE = {
    "mode": 3,
    "mux": 0,            # 0 = 混合，1 = 独显直连
    "kb_disabled": False,
    "kb_rainbow": False,
    "kb_level": 3,       # 0-4
    "kb_color": [50, 28, 12],  # EC 0-50
    "listen": False,
}

# 已连接的客户端（用于推送 mode-changed 事件）：[(wfile, lock), ...]
CLIENTS = []
CLIENTS_LOCK = threading.Lock()


def broadcast(ev: dict) -> None:
    """向所有已连接客户端推送事件（模拟 serve 的 mode-changed）"""
    line = (json.dumps(ev, ensure_ascii=False) + "\n").encode()
    with CLIENTS_LOCK:
        dead = []
        for wf, lock in CLIENTS:
            with lock:
                try:
                    wf.write(line)
                    wf.flush()
                except Exception:
                    dead.append((wf, lock))
        for c in dead:
            if c in CLIENTS:
                CLIENTS.remove(c)


def simulate_key_press(delay: float = 1.2) -> None:
    """模拟按下性能模式键：EC 自行改档，守护进程按 1→2→3 循环推进并广播事件。"""
    def run():
        time.sleep(delay)
        if not STATE["listen"]:
            return
        frm = STATE["mode"]
        to = MODE_CYCLE.get(frm, 1)
        STATE["mode"] = to
        ev = {
            "event": "mode-changed",
            "from": {"index": frm, "name": MODE_NAMES[frm]},
            "to": {"index": to, "name": MODE_NAMES[to]},
        }
        print(f"  => event {ev}", flush=True)
        broadcast(ev)

    threading.Thread(target=run, daemon=True).start()


def status() -> dict:
    bits = MODE_BITS[STATE["mode"]]
    return {
        "mode": STATE["mode"],
        "name": MODE_NAMES[STATE["mode"]],
        "bits": f"{bits:02X}",
        "backend": "ec-direct(mock)",
        "detail": f"EC 0x0751 = 0x{bits:02X}（{MODE_NAMES[STATE['mode']]}），风扇增强 BIT6 = 0",
    }


def gpu_status() -> dict:
    return {
        "var": "UniWillVariable",
        "mux_raw": STATE["mux"],
        "interpreted": "hybrid" if STATE["mux"] == 0 else "dgpu",
        "payload_len": 188,
    }


def kb_status() -> dict:
    r, g, b = STATE["kb_color"]
    return {
        "white_only": False,
        "disabled": STATE["kb_disabled"],
        "brightness_level": STATE["kb_level"],
        "brightness_max": 4,
        "rainbow": STATE["kb_rainbow"],
        "color": {"r": r, "g": g, "b": b},
    }


def clamp8(v: int) -> int:
    return max(0, min(255, int(v)))


def parse_color(value: str):
    v = value.strip().lstrip("#")
    if len(v) == 6:
        return int(v[0:2], 16), int(v[2:4], 16), int(v[4:6], 16)
    if v == "red":
        return 255, 0, 0
    if v == "white":
        return 255, 255, 255
    if v in ("off", "none"):
        return 0, 0, 0
    return 255, 128, 0


def ec_read(addr: str, count: int) -> dict:
    base = int(addr, 16)
    values = []
    for i in range(count):
        a = base + i
        if a == 0x751:
            v = MODE_BITS[STATE["mode"]]
        elif a == 0x78C:
            v = (STATE["kb_level"] << 5) | (2 if STATE["kb_disabled"] else 0)
        elif a == 0x769:
            v = STATE["kb_color"][0]
        elif a == 0x76A:
            v = STATE["kb_color"][1]
        elif a == 0x76B:
            v = STATE["kb_color"][2]
        elif a == 0x767:
            v = (0x80 if STATE["kb_rainbow"] else 0) | 0x20
        else:
            v = 0
        values.append({"addr": f"{a:04X}", "value": v})
    return {"addr": addr, "values": values}


def handle(req: dict) -> tuple[bool, dict]:
    cmd = req.get("cmd")
    value = req.get("value")

    if cmd == "help":
        return True, {"commands": []}

    if cmd == "status":
        return True, status()

    if cmd == "mode":
        try:
            idx = int(value)
        except (TypeError, ValueError):
            names = {"office": 1, "game": 2, "turbo": 3, "custom": 4}
            idx = names.get(str(value).lower(), 0)
        if idx not in MODE_BITS:
            return False, {"error": f"未知模式: {value}"}
        STATE["mode"] = idx
        return True, {"mode": idx, "name": MODE_NAMES[idx], "bits": f"{MODE_BITS[idx]:02X}"}

    if cmd == "gpu":
        action = req.get("action", "status")
        if action == "status":
            return True, gpu_status()
        if action in ("hybrid", "dgpu"):
            STATE["mux"] = 0 if action == "hybrid" else 1
            return True, {"written": STATE["mux"], "unchanged": False, "reboot_required": True}
        return False, {"error": f"未知 gpu 动作: {action}"}

    if cmd == "kb":
        action = req.get("action", "status")
        if action == "status":
            return True, kb_status()
        if action == "color":
            r, g, b = parse_color(str(value))
            STATE["kb_color"] = [r * 50 // 255, g * 50 // 255, b * 50 // 255]
            STATE["kb_rainbow"] = False
            return True, {"r": STATE["kb_color"][0], "g": STATE["kb_color"][1], "b": STATE["kb_color"][2]}
        if action == "rainbow":
            STATE["kb_rainbow"] = str(value) in ("on", "1", "开")
            return True, {"rainbow": STATE["kb_rainbow"]}
        if action == "brightness":
            try:
                pct = int(value)
            except (TypeError, ValueError):
                return False, {"error": "无效亮度"}
            STATE["kb_level"] = min(4, (pct * 4 + 50) // 100)
            return True, {"percent": pct, "level": STATE["kb_level"]}
        if action in ("on", "off"):
            STATE["kb_disabled"] = action == "off"
            return True, {"enabled": not STATE["kb_disabled"]}
        if action == "reset":
            # 与真实后端一致：只把固件默认色抄回颜色寄存器，不动亮度/开关
            STATE["kb_color"] = [50, 50, 50]
            r, g, b = STATE["kb_color"]
            return True, {"reset": True, "color": {"r": r, "g": g, "b": b}}
        return False, {"error": f"未知 kb 动作: {action}"}

    if cmd == "ec-read":
        try:
            count = int(req.get("count", 1))
        except (TypeError, ValueError):
            count = 1
        return True, ec_read(str(req.get("addr", "0x751")), max(1, min(32, count)))

    if cmd == "listen":
        enable = bool(req.get("enable"))
        STATE["listen"] = enable
        if enable:
            # 真实守护进程会轮询 EC 并在按键后推进档位；这里用一次性延时事件代替，
            # 便于在没有硬件时验证「事件 → 界面」链路。
            simulate_key_press()
        return True, {"listening": enable, "owners": 1 if enable else 0}

    return False, {"error": f"未知命令: {cmd}"}


class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        wlock = threading.Lock()
        with CLIENTS_LOCK:
            CLIENTS.append((self.wfile, wlock))
        self.wfile.write(b'{"event":"hello","version":"0.6.0-mock"}\n')
        self.wfile.flush()
        for line in self.rfile:
            line = line.strip()
            if not line:
                continue
            try:
                req = json.loads(line)
            except json.JSONDecodeError as e:
                resp = {"id": None, "ok": False, "error": f"JSON 解析失败: {e}"}
            else:
                print(f"  <- {req}", flush=True)
                ok, payload = handle(req)
                if ok:
                    resp = {"id": req.get("id"), "ok": True, "data": payload}
                else:
                    resp = {"id": req.get("id"), "ok": False, "error": payload.get("error", "错误")}
            with wlock:
                self.wfile.write((json.dumps(resp, ensure_ascii=False) + "\n").encode())
                self.wfile.flush()

        with CLIENTS_LOCK:
            CLIENTS[:] = [c for c in CLIENTS if c[0] is not self.wfile]
        print("  -- client disconnected", flush=True)


class Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True

    def get_request(self):
        request, _ = super().get_request()
        return request, ("local", 0)


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "/tmp/firebat-cc-mock.sock"
    if os.path.exists(path):
        os.remove(path)
    server = Server(path, Handler)
    os.chmod(path, 0o666)
    print(f"mock firebat-cc serve: {path}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        if os.path.exists(path):
            os.remove(path)


if __name__ == "__main__":
    main()
