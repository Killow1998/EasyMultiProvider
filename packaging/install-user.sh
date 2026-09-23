#!/bin/sh
# Install or update the extracted Linux build in the current user's directories.
set -eu

if [ "$(uname -s)" != Linux ] || [ "$(id -u)" = 0 ]; then
    echo 'Run this installer as your desktop user on Linux, without sudo.' >&2
    exit 1
fi

source_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd -P)
data_root=${XDG_DATA_HOME:-"$HOME/.local/share"}
config_root=${XDG_CONFIG_HOME:-"$HOME/.config"}
for root in "$data_root" "$config_root" "$HOME"; do
    case "$root" in
        /*) ;;
        *) echo 'HOME and XDG directories must be absolute paths.' >&2; exit 1 ;;
    esac
done

install_dir=$data_root/easy-multi-provider
config_dir=$config_root/easy-multi-provider
binary=$install_dir/EMP
launcher=$HOME/.local/bin/EMP
desktop=$data_root/applications/easy-multi-provider-user.desktop
quoted_binary=$(printf '%s' "$binary" | sed "s/'/'\\\\''/g")
desktop_exec=$(printf '%s' "$launcher" | sed 's/\\/\\\\/g; s/"/\\"/g; s/`/\\`/g; s/\$/\\$/g; s/%/%%/g')
expected_launcher=$(printf "#!/bin/sh\nexec '%s' \"\$@\"\n" "$quoted_binary")

test -f "$source_dir/EMP" && test ! -L "$source_dir/EMP"
test -f "$source_dir/easy-multi-provider.svg"

if [ -e "$install_dir" ] && [ ! -d "$install_dir" ]; then
    echo "Install path is not a directory: $install_dir" >&2
    exit 1
fi
if [ -L "$install_dir" ] || [ -L "$binary" ] || [ -L "$launcher" ] || [ -L "$desktop" ]; then
    echo 'A managed install path is a symbolic link; refusing to replace it.' >&2
    exit 1
fi

if [ -e "$binary" ]; then
    if ! "$binary" --version 2>/dev/null | grep -q '^EMP [0-9]'; then
        echo "Existing file is not a recognized EMP install: $binary" >&2
        exit 1
    fi
    if [ -e "$launcher" ] && [ "$(cat "$launcher")" != "$expected_launcher" ]; then
        echo "The existing launcher does not point to this EMP install: $launcher" >&2
        exit 1
    fi
    if [ -e "$desktop" ] && ! grep -Fxq "Exec=\"$desktop_exec\"" "$desktop"; then
        echo "The existing desktop entry does not point to this EMP install: $desktop" >&2
        exit 1
    fi
    if command -v pgrep >/dev/null 2>&1 && pgrep -x EMP >/dev/null 2>&1; then
        echo 'EMP is running. Close it, then run the installer again.' >&2
        exit 1
    fi
    install_action=updated
else
    for target in "$launcher" "$desktop"; do
        if [ -e "$target" ]; then
            echo "Already exists: $target. Please inspect it before installing." >&2
            exit 1
        fi
    done
    install_action=installed
fi

mkdir -p -- "$install_dir" "$config_dir" "$HOME/.local/bin" "$data_root/applications"
staged_binary=$install_dir/.EMP.new.$$
trap 'rm -f -- "$staged_binary"' 0 HUP INT TERM
cp -- "$source_dir/EMP" "$staged_binary"
chmod 755 "$staged_binary"
mv -f -- "$staged_binary" "$binary"
cp -- "$source_dir/easy-multi-provider.svg" "$install_dir/easy-multi-provider.svg"

if [ ! -e "$launcher" ]; then
    printf "#!/bin/sh\nexec '%s' \"\$@\"\n" "$quoted_binary" > "$launcher"
    chmod 755 "$launcher"
fi

if [ ! -e "$desktop" ]; then
    cat > "$desktop" <<EOF
[Desktop Entry]
Type=Application
Name=EMP (User)
Comment=Local multi-provider control plane for Codex
Exec="$desktop_exec"
Icon=$install_dir/easy-multi-provider.svg
Terminal=true
Categories=Development;
StartupNotify=true
EOF
fi

printf 'EMP %s: %s\nConfiguration: %s/config.json\nStart from the application menu or run: %s\n' \
    "$install_action" "$binary" "$config_dir" "$launcher"
