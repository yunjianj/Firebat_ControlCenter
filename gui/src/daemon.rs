//! GUI 启动时复用已有服务，否则通过系统授权启动同目录后端。

use crate::client::Client;
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};

extern "C" {
    fn geteuid() -> u32;
}

fn available(socket: &Path) -> Result<bool, String> {
    match UnixStream::connect(socket) {
        Ok(_) => Ok(true),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(false)
        }
        Err(e) => Err(format!("连接 {} 失败: {e}", socket.display())),
    }
}

type Completion = Arc<Mutex<mpsc::Receiver<Result<ExitStatus, String>>>>;
struct Owned {
    input: ChildStdin,
    done: Completion,
}

#[derive(Default)]
pub struct Session {
    stopping: AtomicBool,
    owned: Mutex<Option<Owned>>,
}
impl Session {
    fn manage(&self, mut child: Child) -> Result<Completion, String> {
        let input = child.stdin.take().ok_or("后端生命周期管道未建立")?;
        let (tx, rx) = mpsc::channel();
        let done = Arc::new(Mutex::new(rx));
        std::thread::spawn(move || {
            let _ = tx.send(child.wait().map_err(|e| e.to_string()));
        });
        let mut owned = self.owned.lock().unwrap();
        if self.stopping.load(Ordering::Acquire) {
            drop(input);
        } else {
            *owned = Some(Owned {
                input,
                done: done.clone(),
            });
        }
        Ok(done)
    }
    pub fn is_owned(&self) -> bool {
        self.owned.lock().unwrap().is_some()
    }
    pub fn stop(&self) -> Result<(), String> {
        self.stopping.store(true, Ordering::Release);
        let owned = self.owned.lock().unwrap().take();
        if let Some(Owned { input, done }) = owned {
            drop(input); // root 后端在 stdin EOF 后走正常退出和 EC 写权限清理。
            match done.lock().unwrap().recv_timeout(Duration::from_secs(10)) {
                Ok(Ok(status)) if status.success() => Ok(()),
                Ok(Ok(status)) => Err(format!("后端退出状态：{status}")),
                Ok(Err(error)) => Err(error),
                Err(mpsc::RecvTimeoutError::Disconnected) => Ok(()),
                Err(_) => Err("等待后端退出超时；管道已关闭，后端将在当前操作结束后退出".into()),
            }
        } else {
            Ok(())
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        self.owned.get_mut().unwrap().take();
    }
}

fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|e| format!("生成后端随机凭证失败: {e}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// 返回 true 表示本次启动了后端，false 表示复用已有后端。
fn ensure_with(
    socket: &Path,
    start: impl FnOnce() -> Result<Child, String>,
    timeout: Duration,
    session: &Session,
) -> Result<bool, String> {
    if available(socket)? {
        return Ok(false);
    }
    if session.stopping.load(Ordering::Acquire) {
        return Err("GUI 已退出，取消启动后端".into());
    }
    let done = session.manage(start()?)?;
    let deadline = Instant::now() + timeout;
    loop {
        if session.stopping.load(Ordering::Acquire) {
            return Err("GUI 已退出，取消启动后端".into());
        }
        match available(socket) {
            Ok(true) => {
                return Ok(true);
            }
            Err(e) => {
                return Err(e);
            }
            Ok(false) => {}
        }
        match done.lock().unwrap().try_recv() {
            Ok(Ok(status)) => {
                // 多 GUI 同时启动时，另一个实例可能已经完成启动。
                if available(socket)? {
                    return Ok(false);
                }
                return Err(match status.code() {
                    Some(126) => "已取消后端启动授权".into(),
                    Some(127) => "后端授权失败：请检查 polkit 服务及桌面认证代理".into(),
                    _ => format!("后端启动失败（{status}）：请检查终端输出，或手动运行同目录 firebat-cc serve"),
                });
            }
            Ok(Err(e)) => return Err(format!("检查后端进程失败: {e}")),
            Err(mpsc::TryRecvError::Empty) => {}
            Err(_) => return Err("后端启动进程已结束".into()),
        }
        if Instant::now() >= deadline {
            let _ = session.stop();
            return Err("等待后端启动超时；请完成或取消系统授权窗口，再重试连接".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn ensure_running(socket: &Path, client: &Client, session: &Session) -> Result<bool, String> {
    let result = ensure_with(
        socket,
        || {
            let exe =
                std::env::current_exe().map_err(|e| format!("定位 GUI 可执行文件失败: {e}"))?;
            let backend = exe
                .parent()
                .ok_or("无法确定 GUI 所在目录")?
                .join("firebat-cc");
            if !backend.is_file() {
                return Err(format!(
                    "未找到同目录后端 {}，请将 firebat-cc 与 GUI 放在同一文件夹",
                    backend.display()
                ));
            }
            let help = Command::new(&backend).arg("--help").output()
                .map_err(|e| format!("检查同目录后端能力失败: {e}"))?;
            if !help.status.success() || !String::from_utf8_lossy(&help.stdout).contains("--exit-on-stdin-close") {
                return Err("同目录后端不支持 GUI 生命周期绑定，请同步更新 firebat-cc 与 firebat-cc-gui".into());
            }
            // pkexec 会改变工作目录，因此传入绝对 socket 路径。
            let socket: PathBuf = if socket.is_absolute() {
                socket.into()
            } else {
                std::env::current_dir()
                    .map_err(|e| e.to_string())?
                    .join(socket)
            };
            let mut command = if unsafe { geteuid() } == 0 {
                Command::new(&backend)
            } else {
                let mut command = Command::new("pkexec");
                command.arg("--disable-internal-agent").arg(&backend);
                command
            };
            let token = random_token()?;
            client.set_auth_token(token.clone());
            command.arg("serve").arg(socket).arg("--auth-token").arg(token)
                .arg("--exit-on-stdin-close").stdin(Stdio::piped());
            command.spawn().map_err(|e| {
                format!("启动后端失败: {e}；自动提权需要 pkexec 和桌面 polkit 认证代理")
            })
        },
        Duration::from_secs(120),
        session,
    ).and_then(|started| {
        client.call("help", serde_json::Map::new())
            .map_err(|e| format!("后端连接验证失败：{e}；手动启动的验证后端请通过 FIREBAT_CC_TOKEN 或 --auth-token 提供凭证"))?;
        Ok(started)
    });
    if result.is_err() {
        let _ = session.stop();
        client.disconnect();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn random_credentials_and_owned_process_lifetime() {
        let first = random_token().unwrap();
        let second = random_token().unwrap();
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(first, second);
        let session = Session::default();
        // cat 只在标准输入关闭后退出，模拟后端的父进程生命周期绑定。
        let child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let done = session.manage(child).unwrap();
        assert!(session.is_owned());
        assert!(matches!(
            done.lock().unwrap().try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        session.stop().unwrap();
        assert!(!session.is_owned());
        let stopped = Session::default();
        stopped.stop().unwrap();
        let child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let done = stopped.manage(child).unwrap();
        assert!(done
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .success());
    }
    #[test]
    fn existing_backend_is_not_owned_or_stopped() {
        let dir = std::env::temp_dir().join(format!("firebat-managed-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("mock.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let session = Session::default();
        assert!(!ensure_with(
            &socket,
            || panic!("不应重启已有后端"),
            Duration::from_secs(1),
            &session
        )
        .unwrap());
        session.stop().unwrap();
        assert!(UnixStream::connect(&socket).is_ok());
        drop(listener);
        let managed = Session::default();
        let mut replacement = None;
        assert!(ensure_with(
            &socket,
            || {
                std::fs::remove_file(&socket).unwrap();
                replacement = Some(std::os::unix::net::UnixListener::bind(&socket).unwrap());
                Command::new("cat")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .spawn()
                    .map_err(|e| e.to_string())
            },
            Duration::from_secs(1),
            &managed
        )
        .unwrap());
        managed.stop().unwrap();
        drop(replacement);
        std::fs::remove_file(socket).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
