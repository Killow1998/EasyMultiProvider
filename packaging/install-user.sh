#!/bin/sh
# Install the extracted Linux archive without changing system files or accounts.
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
launcher=$HOME/.local/bin/EMP
desktop=$data_root/applications/easy-multi-provider-user.desktop

# Reinstallation must not replace a running EMP or a different launcher.
for target in "$install_dir/EMP" "$launcher" "$desktop"; do
    if [ -e "$target" ] || [ -L "$target" ]; then
        echo "Already exists: $target. Use EMP's Check updates for an existing user installation." >&2
        exit 1
    fi
done
test -f "$source_dir/EMP"
test -f "$source_dir/easy-multi-provider.svg"
mkdir -p -- "$install_dir" "$config_dir" "$HOME/.local/bin" "$data_root/applications"
cp -- "$source_dir/EMP" "$install_dir/EMP"
chmod 755 "$install_dir/EMP"
cp -- "$source_dir/easy-multi-provider.svg" "$install_dir/easy-multi-provider.svg"

# The shell launcher executes the real binary path, so the updater does not
# attempt to replace a symlink. Configuration stays in the normal user location.
quoted_binary=$(printf '%s' "$install_dir/EMP" | sed "s/'/'\\\\''/g")
printf "#!/bin/sh\nexec '%s' \"\$@\"\n" "$quoted_binary" > "$launcher"
chmod 755 "$launcher"

# Desktop Exec has its own escaping rules, including percent field codes.
desktop_exec=$(printf '%s' "$launcher" | sed 's/\\/\\\\/g; s/"/\\"/g; s/`/\\`/g; s/\$/\\$/g; s/%/%%/g')
cat > "$desktop" <<EOF
[Desktop Entry]
Type=Application
Name=EMP
Comment=Local multi-provider control plane for Codex
Exec="$desktop_exec"
Icon=$install_dir/easy-multi-provider.svg
Terminal=true
Categories=Development;
StartupNotify=true
EOF
printf 'Installed: %s\nConfiguration: %s/config.json\nStart from the application menu or run: %s\n' "$install_dir/EMP" "$config_dir" "$launcher"
