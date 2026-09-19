#!/usr/bin/env bash
# ==============================================================================
# RustDesk Wayland Native Server Launcher (Niri / GNOME / Arch Linux)
# ==============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# 1. Setup local pkg-config for libyuv dynamic library if missing
mkdir -p /tmp/pkgconfig
if [[ ! -f /tmp/pkgconfig/libyuv.pc ]]; then
    cat << 'EOF' > /tmp/pkgconfig/libyuv.pc
prefix=/usr
exec_prefix=${prefix}
libdir=${prefix}/lib
includedir=${prefix}/include

Name: libyuv
Description: YUV conversion and scaling functionality library
Version: 0.0.1
Libs: -L${libdir} -lyuv
Cflags: -I${includedir}
EOF
fi

# 2. Environment variables for modern Arch Linux GCC 14+ and pkg-config
export PKG_CONFIG_PATH="/tmp/pkgconfig:/usr/lib/pkgconfig:/usr/share/pkgconfig:${PKG_CONFIG_PATH:-}"
export CXXFLAGS="-include cstdint ${CXXFLAGS:-}"

# 3. Verify /dev/uinput access
if [[ ! -w /dev/uinput ]]; then
    echo "[!] Warning: /dev/uinput is not writable by current user ($USER)."
    echo "[!] Direct uinput creation may require root ACLs or membership in the 'input' group:"
    echo "    pkexec setfacl -m u:$USER:rw /dev/uinput"
fi

# 4. Run RustDesk with specified arguments (default: --server)
MODE="${1:---server}"
shift || true

echo "=== Starting RustDesk Wayland Server ==="
echo "Mode: $MODE $@"
echo "Compositor: ${WAYLAND_DISPLAY:-wayland-1} ($XDG_CURRENT_DESKTOP)"

exec cargo run --features linux-pkg-config -- "$MODE" "$@"
