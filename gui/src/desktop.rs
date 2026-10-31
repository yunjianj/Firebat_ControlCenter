//! Linux 桌面身份与用户级图标注册。Wayland 从 app_id 对应的 desktop 文件查找图标。

use std::path::{Path, PathBuf};

pub const APP_ID: &str = "firebat-cc-gui";
const ICON: &[u8] = include_bytes!("../ui/assets/logo.png");

fn exec_argument(executable: &Path) -> Result<String, String> {
    let path = executable
        .to_str()
        .ok_or("GUI 路径不是有效 UTF-8，无法注册桌面入口")?;
    if path.contains(['\n', '\r', '\0']) {
        return Err("GUI 路径包含桌面入口不支持的控制字符".into());
    }
    // 先应用 Exec 引号转义，再应用 desktop 字符串转义；% 不能被解释成字段代码。
    let mut quoted = String::new();
    for ch in path.chars() {
        match ch {
            '\\' | '"' | '$' | '`' => {
                quoted.push('\\');
                quoted.push(ch);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(ch),
        }
    }
    Ok(format!("\"{}\"", quoted.replace('\\', "\\\\")))
}

fn write_changed(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if std::fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(());
    }
    let parent = path.parent().ok_or("桌面资源路径没有父目录")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("创建 {} 失败: {e}", parent.display()))?;
    std::fs::write(path, bytes).map_err(|e| format!("写入 {} 失败: {e}", path.display()))
}

fn register_at(data_dir: &Path, executable: &Path) -> Result<(), String> {
    let exec = exec_argument(executable)?;
    let desktop = format!("[Desktop Entry]\nType=Application\nName=Firebat Control Center\nName[zh_CN]=Firebat 控制中心\nComment=Firebat GX performance and keyboard control\nExec={exec} --toggle-window\nIcon={APP_ID}\nTerminal=false\nCategories=Settings;HardwareSettings;\nStartupWMClass={APP_ID}\nStartupNotify=true\n");
    // 先安装图标，避免新 desktop 条目被桌面索引时尚无对应资源。
    write_changed(
        &data_dir.join(format!("icons/hicolor/256x256/apps/{APP_ID}.png")),
        ICON,
    )?;
    write_changed(
        &data_dir.join(format!("applications/{APP_ID}.desktop")),
        desktop.as_bytes(),
    )
}

pub fn register() -> Result<(), String> {
    if unsafe { libc::geteuid() } == 0 {
        return Err("请以普通用户运行 GUI，以便注册当前桌面用户的图标".into());
    }
    let data_dir = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or("无法确定用户桌面资源目录（XDG_DATA_HOME / HOME 未设置）")?;
    let executable = std::env::current_exe().map_err(|e| format!("定位 GUI 失败: {e}"))?;
    register_at(&data_dir, &executable)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn desktop_registration_tracks_executable_and_icon() {
        let dir = std::env::temp_dir().join(format!("firebat-desktop-{}", std::process::id()));
        let first = Path::new("/tmp/Firebat App/firebat-cc-gui");
        register_at(&dir, first).unwrap();
        let desktop_path = dir.join("applications/firebat-cc-gui.desktop");
        let desktop = std::fs::read_to_string(&desktop_path).unwrap();
        assert!(desktop.contains("Exec=\"/tmp/Firebat App/firebat-cc-gui\" --toggle-window\n"));
        assert!(desktop.contains("Icon=firebat-cc-gui\n"));
        assert!(desktop.contains("StartupWMClass=firebat-cc-gui\n"));
        assert_eq!(
            std::fs::read(dir.join("icons/hicolor/256x256/apps/firebat-cc-gui.png")).unwrap(),
            ICON
        );
        let old = std::fs::metadata(&desktop_path)
            .unwrap()
            .modified()
            .unwrap();
        register_at(&dir, first).unwrap();
        assert_eq!(
            old,
            std::fs::metadata(&desktop_path)
                .unwrap()
                .modified()
                .unwrap()
        );
        register_at(&dir, Path::new("/tmp/new/firebat-cc-gui")).unwrap();
        assert!(std::fs::read_to_string(&desktop_path)
            .unwrap()
            .contains("Exec=\"/tmp/new/firebat-cc-gui\""));
        assert!(exec_argument(Path::new("/tmp/bad\nname")).is_err());
        assert_eq!(
            exec_argument(Path::new("/tmp/a%f")).unwrap(),
            "\"/tmp/a%%f\""
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
