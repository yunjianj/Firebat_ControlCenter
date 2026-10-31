//! firebat-cc serve 的 Unix domain socket 客户端。
//!
//! 协议：每行一个 JSON。请求 `{"id":N,"cmd":"...",...}`，
//! 响应 `{"id":N,"ok":bool,"data"|"error":...}`，
//! 事件 `{"event":"...",...}`（无 id）。
//!
//! 所有参数按协议要求以**字符串**形式发送（服务端只读 as_str），
//! 例外：`listen` 的 `enable` 服务端按 bool 解析，需传 `Value::Bool`。
//!
//! 连接建立后由**读线程**独占读取 socket：响应按 id 交给等待中的调用方，
//! 事件（mode-changed 等）推给界面。这样界面在空闲时也能第一时间收到推送，
//! 不必等下一次轮询把事件"捎带"回来。

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DEFAULT_SOCKET: &str = "/run/firebat-cc.sock";
const CALL_TIMEOUT: Duration = Duration::from_secs(8);

pub fn resolve_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("FIREBAT_CC_SOCK") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--socket" {
            if let Some(p) = args.next() {
                return PathBuf::from(p);
            }
        } else if let Some(rest) = a.strip_prefix("--socket=") {
            return PathBuf::from(rest);
        }
    }
    PathBuf::from(DEFAULT_SOCKET)
}

struct Conn {
    writer: Mutex<UnixStream>,
    /// 等待响应的调用方，key 是请求 id
    pending: Mutex<HashMap<u64, Sender<Result<Value, String>>>>,
    dead: AtomicBool,
}

impl Conn {
    /// 连接断开时把还在等待的调用立即失败掉，避免各自等到超时
    fn fail_pending(&self, err: &str) {
        let mut pend = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        for (_, tx) in pend.drain() {
            let _ = tx.send(Err(err.to_string()));
        }
    }
}

pub struct Client {
    path: PathBuf,
    conn: Mutex<Option<Arc<Conn>>>,
    next_id: AtomicU64,
    events: Mutex<Option<Sender<Value>>>,
}

impl Client {
    pub fn new(path: PathBuf, events: Sender<Value>) -> Client {
        Client {
            path,
            conn: Mutex::new(None),
            next_id: AtomicU64::new(1),
            events: Mutex::new(Some(events)),
        }
    }

    fn connect(&self) -> Result<Arc<Conn>, String> {
        // 没有 socket 文件时给出更明确的提示
        if !self.path.exists() {
            return Err(format!(
                "守护进程未运行（{} 不存在）",
                self.path.display()
            ));
        }
        let stream = UnixStream::connect(&self.path)
            .map_err(|e| format!("连接 {} 失败: {e}", self.path.display()))?;
        let writer = stream.try_clone().map_err(|e| e.to_string())?;
        let conn = Arc::new(Conn {
            writer: Mutex::new(writer),
            pending: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
        });

        // 读线程：本连接唯一的读取方
        let reader_conn = conn.clone();
        let events = self.events.lock().ok().and_then(|g| g.clone());
        let path = self.path.clone();
        std::thread::spawn(move || {
            let mut buf = BufReader::new(stream);
            let mut line = String::new();
            loop {
                line.clear();
                match buf.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let text = line.trim();
                if text.is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(text) else {
                    continue;
                };
                // 事件（无 id）：转给界面线程
                if v.get("event").and_then(|e| e.as_str()).is_some() {
                    if let Some(tx) = &events {
                        let _ = tx.send(v);
                    }
                    continue;
                }
                let Some(id) = v.get("id").and_then(|i| i.as_u64()) else {
                    continue;
                };
                let waiter = reader_conn
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                if let Some(tx) = waiter {
                    let ok = v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
                    let res = if ok {
                        Ok(v.get("data").cloned().unwrap_or(Value::Null))
                    } else {
                        Err(v
                            .get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("未知错误")
                            .to_string())
                    };
                    let _ = tx.send(res);
                }
            }
            // 读线程结束 = 连接不可用
            reader_conn.dead.store(true, Ordering::Relaxed);
            reader_conn.fail_pending("守护进程关闭了连接");
            eprintln!("与 {} 的连接已断开", path.display());
        });

        Ok(conn)
    }

    /// 取一个可用连接，必要时（重）连。
    fn link(&self) -> Result<Arc<Conn>, String> {
        let mut guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = guard.as_ref() {
            if !c.dead.load(Ordering::Relaxed) {
                return Ok(c.clone());
            }
        }
        let c = self.connect()?;
        *guard = Some(c.clone());
        Ok(c)
    }

    /// 发送一次命令并等待配对响应。阻塞调用，需在后台线程执行。
    pub fn call(&self, cmd: &str, params: Map<String, Value>) -> Result<Value, String> {
        let conn = self.link()?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let mut obj = Map::new();
        obj.insert("id".to_string(), json!(id));
        obj.insert("cmd".to_string(), json!(cmd));
        for (k, v) in params {
            obj.insert(k, v);
        }
        let mut line = Value::Object(obj).to_string();
        line.push('\n');

        let (tx, rx) = mpsc::channel();
        conn.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);

        let write_res = conn
            .writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(line.as_bytes());
        if let Err(e) = write_res {
            conn.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            conn.dead.store(true, Ordering::Relaxed);
            return Err(format!("发送请求失败: {e}"));
        }

        match rx.recv_timeout(CALL_TIMEOUT) {
            Ok(res) => res,
            Err(_) => {
                conn.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                Err("等待响应超时（守护进程无响应）".to_string())
            }
        }
    }
}
