#!/bin/sh
# Interactive, verified Linux installer for the latest stable EMP release.
set -eu

REPOSITORY=Killow1998/EasyMultiProvider
LATEST_URL=https://github.com/$REPOSITORY/releases/latest
ARCHIVE_NAME=EMP-linux-x86_64.tar.gz
TTY=/dev/tty

if [ ! -r "$TTY" ] || [ ! -w "$TTY" ]; then
    echo 'EMP Linux installer needs an interactive terminal.' >&2
    exit 1
fi
if [ "$(uname -s)" != Linux ]; then
    echo 'This installer supports Linux x86_64 only.' >&2
    exit 1
fi
case "$(uname -m)" in
    x86_64|amd64) ;;
    *) echo 'This release currently supports Linux x86_64 only.' >&2; exit 1 ;;
esac
if [ "$(id -u)" = 0 ]; then
    echo 'Run EMP as your desktop user. Do not use sudo for this installer.' >&2
    exit 1
fi
if [ -z "${HOME:-}" ] || [ "${HOME#/}" = "$HOME" ]; then
    echo 'HOME must be set to an absolute path.' >&2
    exit 1
fi

if [ -t 1 ] && [ -n "${TERM:-}" ] && [ "$TERM" != dumb ]; then
    RESET=$(printf '\033[0m')
    BOLD=$(printf '\033[1m')
    CYAN=$(printf '\033[38;5;51m')
    MAGENTA=$(printf '\033[38;5;213m')
    GREEN=$(printf '\033[38;5;84m')
    YELLOW=$(printf '\033[38;5;220m')
    MUTED=$(printf '\033[38;5;245m')
else
    RESET= BOLD= CYAN= MAGENTA= GREEN= YELLOW= MUTED=
fi

printf '\n%s╭──────────────────────────────────────────────────────────────╮%s\n' "$CYAN" "$RESET"
printf '%s│%s             %s✦  EASY MULTI-PROVIDER  ✦%s                      %s│%s\n' "$CYAN" "$RESET" "$MAGENTA" "$RESET" "$CYAN" "$RESET"
printf '%s│%s                    Linux installer                           %s│%s\n' "$CYAN" "$RESET" "$CYAN" "$RESET"
printf '%s╰──────────────────────────────────────────────────────────────╯%s\n\n' "$CYAN" "$RESET"
printf '%sChoose language / 選擇語言%s\n' "$BOLD" "$RESET"
printf '  %s1%s  English\n  %s2%s  繁體中文\n' "$GREEN" "$RESET" "$GREEN" "$RESET"
printf 'Select [1/2]: ' > "$TTY"
IFS= read -r language_choice < "$TTY" || exit 1
case "$language_choice" in
    1) LANGUAGE=en ;;
    2) LANGUAGE=zh ;;
    *) printf '\nPlease enter 1 or 2. Run the installer again.\n' >&2; exit 1 ;;
esac

if [ "$LANGUAGE" = zh ]; then
    TITLE='EMP Linux 安裝程式'
    INTRO='安裝到你的使用者目錄；之後可在 EMP 網頁中更新。'
    NEEDS='需要 curl、tar、gzip 與 sha256sum。'
    PKG_FOUND='偵測到系統安裝的 Debian 套件'
    REMOVE_ASK='要移除舊的系統套件嗎？設定檔會保留。 [y/N] '
    REMOVE_LATER='保留系統套件；之後請從應用程式選單選擇 EMP (User)。'
    REMOVE_DONE='系統套件已移除。'
    REMOVE_FAILED='無法移除系統套件；使用者版本仍會安裝。'
    CONFIG_CURRENT='已找到標準使用者設定；若不遷移，會直接沿用。'
    CONFIG_REPLACE='標準使用者設定已存在；若選擇遷移，原有資料會先備份。'
    CONFIG_BACKUP='原有使用者資料已備份至：'
    CONFIG_NONE='沒有找到其他舊版設定。'
    CONFIG_FOUND='找到舊版原始碼設定：'
    CONFIG_SELECT='輸入來源編號，或輸入 0 略過遷移：'
    CONFIG_ASK='要複製這份設定與 state 到標準使用者目錄嗎？原始資料會保留。 [y/N] '
    CONFIG_DONE='設定與本機 state 已複製，來源檔案仍保留。'
    CONFIG_SKIPPED='略過設定遷移；原始資料未變更。'
    FETCH='連線並檢查最新穩定版本'
    DOWNLOAD='下載 Linux 安裝檔'
    VERIFY='驗證 SHA-256 並解壓'
    INSTALL='安裝至使用者目錄'
    RUNNING='偵測到 EMP 正在執行。請先關閉所有 EMP 視窗，再重新執行安裝程式。'
    SUCCESS='安裝完成。請從應用程式選單啟動 EMP (User)。'
    ERROR='安裝失敗；目前設定與原始設定檔均未刪除。'
else
    TITLE='EMP Linux Installer'
    INTRO='Install for your user. Future updates are available from the EMP Web UI.'
    NEEDS='Requires curl, tar, gzip, and sha256sum.'
    PKG_FOUND='A system-managed Debian package was detected'
    REMOVE_ASK='Remove the old system package? Its configuration is kept. [y/N] '
    REMOVE_LATER='The system package stays installed. Choose EMP (User) from the app menu.'
    REMOVE_DONE='The system package was removed.'
    REMOVE_FAILED='Could not remove the system package; the user install will continue.'
    CONFIG_CURRENT='A standard user configuration exists; it will be reused unless you migrate.'
    CONFIG_REPLACE='A user configuration already exists; choosing migration will back it up first.'
    CONFIG_BACKUP='Previous user data was backed up at:'
    CONFIG_NONE='No other legacy source configuration was found.'
    CONFIG_FOUND='Legacy source configuration found:'
    CONFIG_SELECT='Enter a source number, or 0 to skip migration:'
    CONFIG_ASK='Copy this configuration and its state to the user config directory? The source stays in place. [y/N] '
    CONFIG_DONE='Configuration and local state copied; the source remains in place.'
    CONFIG_SKIPPED='Configuration migration skipped; source data was not changed.'
    FETCH='Connect and inspect the latest stable release'
    DOWNLOAD='Download the Linux package'
    VERIFY='Verify SHA-256 and extract'
    INSTALL='Install for this user'
    RUNNING='EMP is running. Close all EMP windows, then run the installer again.'
    SUCCESS='Installation complete. Launch EMP (User) from your application menu.'
    ERROR='Installation failed; existing and source configuration files were kept.'
fi

printf '\n%s%s%s\n%s\n%s\n\n' "$BOLD" "$TITLE" "$RESET" "$INTRO" "$NEEDS"

ask_yes_no() {
    printf '%s' "$1" > "$TTY"
    IFS= read -r answer < "$TTY" || answer=
    case "$answer" in y|Y|yes|YES|Yes) return 0 ;; *) return 1 ;; esac
}

data_root=${XDG_DATA_HOME:-"$HOME/.local/share"}
config_root=${XDG_CONFIG_HOME:-"$HOME/.config"}
for root in "$data_root" "$config_root"; do
    case "$root" in
        /*) ;;
        *) echo 'HOME and XDG directories must be absolute paths.' >&2; exit 1 ;;
    esac
done
config_dir=$config_root/easy-multi-provider
target_config=$config_dir/config.json
migrate_config=
selected_source=

if [ -f "$target_config" ]; then
    printf '%s\n' "$CONFIG_CURRENT"
fi
candidate_file=$(mktemp "${TMPDIR:-/tmp}/emp-config-candidates.XXXXXX")
{
    if [ -f "$PWD/config.json" ] && [ ! -L "$PWD/config.json" ] && [ "$PWD/config.json" != "$target_config" ]; then
        printf '%s\n' "$PWD/config.json"
    fi
    find "$HOME" -maxdepth 6 \
        \( -path "$HOME/.cache" -o -path "$HOME/.local" -o -path "$HOME/.config" \
        -o -path '*/.venv' -o -path '*/node_modules' -o -path '*/.git' \) -prune -o \
        -type f -path '*/EasyMultiProvider/config.json' -print 2>/dev/null || true
} | awk '!seen[$0]++' > "$candidate_file"
candidate_count=$(awk 'END { print NR + 0 }' "$candidate_file")
if [ "$candidate_count" -eq 0 ]; then
    if [ ! -f "$target_config" ]; then printf '%s\n' "$CONFIG_NONE"; fi
else
    printf '\n%s\n' "$CONFIG_FOUND"
    candidate_number=0
    while IFS= read -r candidate; do
        candidate_number=$((candidate_number + 1))
        printf '  %s%s%s  %s\n' "$MAGENTA" "$candidate_number" "$RESET" "$candidate"
    done < "$candidate_file"
    printf '  %s0%s  %s\n' "$MAGENTA" "$RESET" "$CONFIG_SKIPPED"
    printf '%s ' "$CONFIG_SELECT" > "$TTY"
    IFS= read -r config_choice < "$TTY" || config_choice=0
    case "$config_choice" in
        ''|*[!0-9]*) config_choice=0 ;;
    esac
    if [ "$config_choice" -gt 0 ] && [ "$config_choice" -le "$candidate_count" ]; then
        selected_source=$(sed -n "${config_choice}p" "$candidate_file")
        if [ -e "$target_config" ] || [ -e "$config_dir/state" ]; then printf '%s\n' "$CONFIG_REPLACE"; fi
        if ask_yes_no "$CONFIG_ASK"; then
            migrate_config=yes
        else
            printf '%s\n' "$CONFIG_SKIPPED"
        fi
    fi
fi
rm -f -- "$candidate_file"

remove_deb=no
if command -v dpkg-query >/dev/null 2>&1; then
    deb_status=$(dpkg-query -W -f='${Status}' easy-multi-provider 2>/dev/null || true)
    if [ "$deb_status" = 'install ok installed' ]; then
        deb_version=$(dpkg-query -W -f='${Version}' easy-multi-provider 2>/dev/null || true)
        printf '\n%s%s%s · %s\n' "$YELLOW" "$PKG_FOUND" "$RESET" "$deb_version"
        if ask_yes_no "$REMOVE_ASK"; then remove_deb=yes; else printf '%s\n' "$REMOVE_LATER"; fi
    fi
fi

for tool in curl tar gzip sha256sum; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf 'Missing required command: %s\n' "$tool" >&2
        exit 1
    fi
done

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/emp-install.XXXXXX")
cleanup() { rm -rf -- "$work_dir"; }
trap cleanup 0
trap 'cleanup; exit 1' HUP INT TERM

show_step() {
    number=$1
    label=$2
    case "$number" in
        1) bar='━━━○○○○' ;;
        2) bar='━━━━━━○○' ;;
        3) bar='━━━━━━━━━○' ;;
        4) bar='━━━━━━━━━━━━' ;;
        *) bar='━━━━━━━━━━━━' ;;
    esac
    printf '\n%s[%s]%s %s  %s%s%s\n' "$CYAN" "$bar" "$RESET" "$number/4" "$BOLD" "$label" "$RESET"
}

show_step 1 "$FETCH"
# The latest stable release redirects to /releases/tag/vX.Y.Z; drafts and
# prereleases are never "latest".
if ! latest=$(curl --fail --location --silent --show-error --retry 3 --head \
    --output /dev/null --write-out '%{url_effective}' "$LATEST_URL"); then
    echo "$ERROR" >&2
    exit 1
fi
release_tag=${latest##*/releases/tag/}
if [ "$release_tag" = "$latest" ] || ! printf '%s\n' "$release_tag" | grep -Eqx 'v[0-9]+\.[0-9]+\.[0-9]+'; then
    echo 'The latest stable release metadata is invalid.' >&2
    exit 1
fi
release_version=${release_tag#v}
archive_url=https://github.com/$REPOSITORY/releases/download/$release_tag/$ARCHIVE_NAME
if ! curl --fail --location --silent --show-error --retry 3 --max-filesize 1024 \
    "$archive_url.sha256" --output "$work_dir/$ARCHIVE_NAME.sha256"; then
    echo "$ERROR" >&2
    exit 1
fi
read -r release_digest checksum_name < "$work_dir/$ARCHIVE_NAME.sha256" || true
if [ "${checksum_name:-}" != "$ARCHIVE_NAME" ] || ! printf '%s\n' "${release_digest:-}" | grep -Eqx '[a-f0-9]{64}'; then
    echo 'The Linux release archive has no valid SHA-256 digest.' >&2
    exit 1
fi
printf '%s  v%s\n' "$GREEN" "$release_version"

show_step 2 "$DOWNLOAD"
if ! curl --fail --location --retry 3 --progress-bar --max-filesize 536870912 "$archive_url" \
    --output "$work_dir/$ARCHIVE_NAME"; then
    echo "$ERROR" >&2
    exit 1
fi

show_step 3 "$VERIFY"
actual_digest=$(sha256sum "$work_dir/$ARCHIVE_NAME" | awk '{print $1}')
if [ "$actual_digest" != "$release_digest" ]; then
    echo 'Release archive verification failed.' >&2
    exit 1
fi
# Only regular files and directories under EMP/, no duplicates or escapes,
# and at most 1 GiB once expanded.
if ! expanded_size=$(gzip -dc "$work_dir/$ARCHIVE_NAME" | wc -c | tr -d ' ') ||
    [ "$expanded_size" -gt 1073741824 ] ||
    ! tar -tzf "$work_dir/$ARCHIVE_NAME" > "$work_dir/archive-names" ||
    ! tar -tzvf "$work_dir/$ARCHIVE_NAME" > "$work_dir/archive-listing"; then
    echo 'The release archive could not be read.' >&2
    exit 1
fi
if ! awk '{ name = $0; sub(/\/$/, "", name)
        if (name ~ /^\// || name ~ /(^|\/)\.\.(\/|$)/ || (name != "EMP" && name !~ /^EMP\//) || seen[name]++) bad = 1 }
        END { exit bad || NR == 0 }' "$work_dir/archive-names" ||
    ! awk '{ kind = substr($1, 1, 1); if (kind != "-" && kind != "d") bad = 1 } END { exit bad }' "$work_dir/archive-listing"; then
    echo 'The release archive contains an unsafe entry.' >&2
    exit 1
fi
for required in EMP/EMP EMP/install-user.sh EMP/easy-multi-provider.svg; do
    if ! awk -v want="$required" 'substr($1, 1, 1) == "-" && $NF == want { found = 1 } END { exit !found }' "$work_dir/archive-listing"; then
        echo 'The release archive is missing required files.' >&2
        exit 1
    fi
done
mkdir "$work_dir/extracted"
tar -xzf "$work_dir/$ARCHIVE_NAME" -C "$work_dir/extracted"

show_step 4 "$INSTALL"
if command -v pgrep >/dev/null 2>&1 && pgrep -x EMP >/dev/null 2>&1; then
    printf '%s\n' "$RUNNING" >&2
    exit 1
fi
if ! (CDPATH= cd -- "$work_dir/extracted/EMP" && ./install-user.sh); then
    echo "$ERROR" >&2
    exit 1
fi

if [ -n "$migrate_config" ]; then
    mkdir -p -- "$config_dir"
    # The verified EMP binary copies the configuration, its state and the
    # files it references, and prints the backup of anything it replaced.
    if ! backup_dir=$("$work_dir/extracted/EMP/EMP" --emp-migrate-config "$selected_source" "$target_config")
    then
        printf '%s\n' "$ERROR" >&2
        exit 1
    fi
    printf '%s\n' "$CONFIG_DONE"
    if [ -n "$backup_dir" ]; then
        printf '%s %s\n' "$CONFIG_BACKUP" "$backup_dir"
    fi
fi

if [ "$remove_deb" = yes ]; then
    if command -v sudo >/dev/null 2>&1; then
        if sudo apt remove -y easy-multi-provider; then
            printf '%s\n' "$REMOVE_DONE"
        else
            printf '%s\n' "$REMOVE_FAILED" >&2
        fi
    else
        printf '%s\n' "$REMOVE_FAILED" >&2
    fi
fi

printf '\n%s%s%s · v%s\n' "$GREEN" "$SUCCESS" "$RESET" "$release_version"
