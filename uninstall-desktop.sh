#!/bin/sh
# 清除旧版本写入当前用户目录的桌面应用注册。
set -eu

if [ "$(id -u)" -eq 0 ]; then
    printf '%s\n' '请以注册桌面图标的普通用户运行此脚本，不要使用 sudo。' >&2
    exit 1
fi

case ${XDG_DATA_HOME:-} in
    /*) data_dir=$XDG_DATA_HOME ;;
    *)
        if [ -z "${HOME:-}" ]; then
            printf '%s\n' '无法确定用户桌面资源目录：请设置 HOME 或绝对路径 XDG_DATA_HOME。' >&2
            exit 1
        fi
        data_dir=$HOME/.local/share
        ;;
esac

desktop=$data_dir/applications/firebat-cc-gui.desktop
icon=$data_dir/icons/hicolor/256x256/apps/firebat-cc-gui.png

for file in "$desktop" "$icon"; do
    if [ -e "$file" ] || [ -L "$file" ]; then
        rm -f -- "$file"
        printf '已移除：%s\n' "$file"
    else
        printf '不存在，跳过：%s\n' "$file"
    fi
done

# 有些桌面环境需要更新索引；工具或缓存不存在时，桌面环境会自行刷新。
if command -v update-desktop-database >/dev/null 2>&1 && [ -d "$data_dir/applications" ]; then
    update-desktop-database "$data_dir/applications" || printf '%s\n' '提示：桌面应用索引刷新失败，可能需要重新登录。' >&2
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1 && [ -f "$data_dir/icons/hicolor/index.theme" ]; then
    gtk-update-icon-cache -f -t "$data_dir/icons/hicolor" || printf '%s\n' '提示：图标缓存刷新失败，可能需要重新登录。' >&2
fi
printf '%s\n' '清理完成。由 pacman 安装的系统级桌面入口和图标未受影响。'
