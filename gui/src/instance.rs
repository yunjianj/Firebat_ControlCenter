//! 每个桌面会话只运行一个 GUI；任务栏启动请求通过私有 Unix socket 转发。
use crate::AppWindow;
use slint::ComponentHandle;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::{
    fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    net::{UnixListener, UnixStream},
};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

pub struct Instance {
    _lock: File,
    socket: PathBuf,
    listener: Option<UnixListener>,
    stopped: Arc<AtomicBool>,
}

fn directory() -> Result<PathBuf, String> {
    let uid = unsafe { libc::geteuid() };
    let parent = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .filter(|p| {
            std::fs::symlink_metadata(p)
                .is_ok_and(|m| m.is_dir() && m.uid() == uid && m.mode() & 0o077 == 0)
        });
    let dir = parent
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(format!("firebat-cc-gui-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("创建 GUI 会话目录失败: {e}")),
    }
    let m = std::fs::symlink_metadata(&dir).map_err(|e| e.to_string())?;
    if !m.is_dir() || m.uid() != uid || m.mode() & 0o077 != 0 {
        return Err("GUI 会话目录必须由当前用户拥有且权限为 0700".into());
    }
    Ok(dir)
}

impl Instance {
    pub fn claim(toggle: bool, token: Option<String>, ec_debug: bool) -> Result<Option<Self>, String> {
        use std::hash::{Hash, Hasher};
        let mut session = std::collections::hash_map::DefaultHasher::new();
        std::env::var_os("WAYLAND_DISPLAY").hash(&mut session);
        std::env::var_os("DISPLAY").hash(&mut session);
        let base = directory()?.join(format!("{:016x}", session.finish()));
        Self::claim_at(&base, toggle, token, ec_debug)
    }

    fn claim_at(base: &Path, toggle: bool, token: Option<String>, ec_debug: bool) -> Result<Option<Self>, String> {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(base.with_extension("lock"))
            .map_err(|e| e.to_string())?;
        let socket = base.with_extension("sock");
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(format!("获取 GUI 会话锁失败: {error}"));
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            let stream = loop {
                match UnixStream::connect(&socket) {
                    Ok(stream) => break Some(stream),
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                        ) && Instant::now() < deadline =>
                    {
                        // 旧实例可能已移除 socket、正在退出；重新竞争锁，而非只等连接。
                        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }
                            == 0
                        {
                            break None;
                        }
                        std::thread::sleep(Duration::from_millis(50))
                    }
                    Err(e) => return Err(format!("唤回已有 GUI 失败: {e}")),
                }
            };
            if let Some(mut stream) = stream {
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .map_err(|e| e.to_string())?;
                let mut request = serde_json::json!({"toggle":toggle,"token":token,"ec_debug":ec_debug}).to_string();
                request.push('\n');
                stream
                    .write_all(request.as_bytes())
                    .map_err(|e| e.to_string())?;
                return Ok(None);
            }
        }
        if let Ok(metadata) = std::fs::symlink_metadata(&socket) {
            if !metadata.file_type().is_socket() {
                return Err("GUI 会话 socket 路径已被其他文件占用".into());
            }
            std::fs::remove_file(&socket).map_err(|e| e.to_string())?;
        }
        let listener = UnixListener::bind(&socket).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        Ok(Some(Self {
            _lock: lock,
            socket,
            listener: Some(listener),
            stopped: Arc::new(AtomicBool::new(false)),
        }))
    }

    pub fn listen(&mut self, ui: &AppWindow) {
        let listener = self.listener.take().unwrap();
        let weak = ui.as_weak();
        let stopped = self.stopped.clone();
        std::thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                        let mut request = String::new();
                        if stream.take(8192).read_to_string(&mut request).is_err() {
                            continue;
                        }
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(&request) else {
                            continue;
                        };
                        let toggle = value
                            .get("toggle")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let ec_debug = value.get("ec_debug").and_then(|v| v.as_bool()).unwrap_or(false);
                        let token = value
                            .get("token")
                            .and_then(|v| v.as_str())
                            .filter(|s| s.len() <= 4096)
                            .map(str::to_string);
                        let weak = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                if ec_debug {
                                    ui.global::<crate::Nav>().set_ec_debug_enabled(true);
                                }
                                if toggle {
                                    crate::window::toggle(&ui, token);
                                } else {
                                    crate::window::open(&ui, token);
                                }
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(50))
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_forwards_instead_of_starting_another_instance() {
        let base =
            std::env::temp_dir().join(format!("firebat-instance-test-{}", std::process::id()));
        let mut first = Instance::claim_at(&base, false, None, false).unwrap().unwrap();
        assert!(Instance::claim_at(&base, true, Some("test-token".into()), true)
            .unwrap()
            .is_none());
        let (stream, _) = first.listener.take().unwrap().accept().unwrap();
        let mut request = String::new();
        stream.take(8192).read_to_string(&mut request).unwrap();
        let request: serde_json::Value = serde_json::from_str(&request).unwrap();
        assert_eq!(request["toggle"], true);
        assert_eq!(request["token"], "test-token");
        assert_eq!(request["ec_debug"], true);
        // 模拟退出时 socket 已移除，但其他文件描述符稍后才释放锁。
        let retained = first._lock.try_clone().unwrap();
        drop(first);
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(retained);
        });
        assert!(Instance::claim_at(&base, false, None, false).unwrap().is_some());
        release.join().unwrap();
        std::fs::remove_file(base.with_extension("lock")).unwrap();
    }
}
