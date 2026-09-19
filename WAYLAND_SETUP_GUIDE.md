# Guía de Uso: Servidor RustDesk Nativo para Wayland (Arch Linux / Niri)

Esta guía documenta la implementación completa y el uso del soporte nativo para **Wayland** desarrollado en la rama `feat/wayland-native-support` de RustDesk.

---

## 1. Características Implementadas y Verificadas

1. **Captura de Pantalla Desatendida (Promptless ScreenCast):**
   - Integración nativa con la interfaz D-Bus `org.gnome.Mutter.ScreenCast` (soportada por Niri y compositores compatibles).
   - Inicia streams de PipeWire en resolución nativa (ej. **4K 3840x2160**) sin solicitar confirmación interactiva al usuario en cada conexión.
   - Manejo de buffers de PipeWire con elemento de negociación `videoconvert` y fijado estricto de caps (`framerate=0/1`), compatible con modificadores DRM de NVIDIA.

2. **Inyección de Entrada Directa sin Privilegios (`/dev/uinput`):**
   - Dispositivos virtuales creados mediante ioctls directos sobre `/dev/uinput` (`UI_DEV_SETUP`, `UI_DEV_CREATE`).
   - Evita la restricción de lectura en `/dev/input/event*`, permitiendo inyección de teclado y ratón sin privilegios de root cuando `/dev/uinput` tiene ACLs de usuario (predeterminado en Arch Linux vía systemd uaccess).
   - Multi-tier fallback automático:
     1. Servicio root IPC (`rustdesk --service`), si está activo.
     2. Inyección directa en el proceso (`DirectUInputKeyboard` y `DirectUInputMouse`), si `/dev/uinput` es escribible.
     3. Portal `RemoteDesktop` de XDG Desktop Portal, como último recurso.

3. **Pipeline de Codificación de Video en Tiempo Real:**
   - Conversión de formato de píxeles a YUV (I420) y compresión continua con codificador VP9 multihilo.
   - Sincronización de portapapeles en Wayland a través del backend `arboard` (fork rustdesk).

---

## 2. Requisitos del Sistema y Entorno

- **Distribución:** Arch Linux (Kernel 6.19+).
- **Compositor:** Niri 25.11+ (o cualquier compositor con soporte PipeWire / ScreenCast).
- **Librerías del Sistema:**
  - `pipewire`, `wireplumber`, `gst-plugin-pipewire`, `gstreamer`, `gst-plugins-base`.
  - `libyuv`, `opus`, `libva`.

### Variables de Entorno de Compilación
Debido a que Arch Linux incluye versiones modernas de GCC (14+) y librerías dinámicas sin archivos `.pc` para `libyuv`, se deben exportar las siguientes variables:

```bash
mkdir -p /tmp/pkgconfig
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

export PKG_CONFIG_PATH="/tmp/pkgconfig:/usr/lib/pkgconfig:/usr/share/pkgconfig:$PKG_CONFIG_PATH"
export CXXFLAGS="-include cstdint $CXXFLAGS"
```

---

## 3. Ejecución Rápida con el Script Helper

Hemos proporcionado el script [`run_wayland_server.sh`](run_wayland_server.sh) que automatiza la configuración de variables y el arranque del servidor:

```bash
# Iniciar RustDesk en modo servidor dentro de la sesión Wayland activa
./run_wayland_server.sh
```

O para pasar argumentos específicos:
```bash
# Ver versión
./run_wayland_server.sh --version

# Ejecutar cliente interactivo
./run_wayland_server.sh --tray
```

---

## 4. Ejecución de Tests de Verificación

Se han creado dos herramientas de prueba aisladas para validar cada capa del sistema:

### A. Test de Inyección de Entrada Virtual ([`examples/test_wayland_input.rs`](examples/test_wayland_input.rs))
Verifica la creación del dispositivo virtual de ratón y teclado en `/dev/uinput`, y ejecuta movimientos relativos, clics y pulsaciones de teclas:

```bash
CXXFLAGS="-include cstdint" PKG_CONFIG_PATH=/tmp/pkgconfig:/usr/lib/pkgconfig:/usr/share/pkgconfig \
cargo run --example test_wayland_input --features linux-pkg-config
```

### B. Benchmark del Pipeline Completo E2E ([`examples/test_wayland_pipeline.rs`](examples/test_wayland_pipeline.rs))
Inicializa la captura de pantalla nativa, captura 30 fotogramas reales de la pantalla, los convierte a YUV, los codifica con VP9, e inyecta eventos de ratón interactivos simultáneamente:

```bash
CXXFLAGS="-include cstdint" PKG_CONFIG_PATH=/tmp/pkgconfig:/usr/lib/pkgconfig:/usr/share/pkgconfig \
cargo run --example test_wayland_pipeline --features linux-pkg-config
```

---

## 5. Configuración para Arranque Desatendido (Systemd)

Para que el servidor se ejecute automáticamente desde el inicio del sistema:

### Opción 1: Servicio de Usuario (Recomendado para sesión de escritorio)
Crear el archivo `~/.config/systemd/user/rustdesk-server.service`:

```ini
[Unit]
Description=RustDesk Wayland Server
PartOf=graphical-session.target
After=graphical-session.target

[Service]
Type=simple
WorkingDirectory=/home/mat/rustdesk
ExecStart=/home/mat/rustdesk/run_wayland_server.sh --server
Restart=always
RestartSec=5
Environment=WAYLAND_DISPLAY=wayland-1
Environment=XDG_CURRENT_DESKTOP=niri

[Install]
WantedBy=graphical-session.target
```

Habilitar e iniciar con:
```bash
systemctl --user daemon-reload
systemctl --user enable --now rustdesk-server.service
```

### Opción 2: Servicio de Sistema (`rustdesk.service`)
Si se requiere acceso previo al login o gestión global:

```bash
pkexec cp res/rustdesk.service /etc/systemd/system/
pkexec systemctl daemon-reload
pkexec systemctl enable --now rustdesk.service
```
*(Nota: El servicio del sistema utiliza `pkexec` de acuerdo con las políticas locales).*

---

## 6. Solución de Problemas Frecuentes

- **Permisos en `/dev/uinput`:**
  Si aparece el aviso `Permission denied` al abrir `/dev/uinput`, verificar que el usuario tenga acceso:
  ```bash
  getfacl /dev/uinput
  ```
  Si falta el ACL de usuario, otorgarlo con:
  ```bash
  pkexec setfacl -m u:$USER:rw /dev/uinput
  ```
- **Error `Element failed to change its state` en GStreamer:**
  Asegúrese de que no haya otro capturer activo en el mismo hilo. La función [`get_capturer_for_display`](src/server/wayland.rs) debe utilizarse para reutilizar la conexión existente de PipeWire.
