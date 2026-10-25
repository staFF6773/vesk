# Vesk

Reimplementacion del **shell** de [Vesktop](https://github.com/Vencord/Vesktop) en Rust con
[Tauri 2](https://tauri.app). Carga el cliente web de Discord en un webview del sistema e
inyecta [Vencord](https://github.com/Vendicated/Vencord) al inicio del documento.

> No es un fork del codigo Electron de Vesktop. Vencord sigue siendo JavaScript y se ejecuta
> dentro del webview; lo que cambia es el contenedor: de Electron/Chromium a Rust/WebKitGTK.


## Estado

Verificado en NixOS (Hyprland, Wayland nativo y XWayland): compila con Tauri 2.12.5 y
WebKitGTK 2.54.1, carga la pantalla de login de Discord y Vencord queda inyectado
(`window.Vencord` es un objeto y el bundle aplica sus estilos). `cargo test` cubre la
traduccion de `discord://`, el filtro de navegacion, el arte embebido y los flags de CEF.

## Rendimiento (leer antes de tocar el entorno)

WebKitGTK solo compone por GPU en **Wayland nativo**. Bajo XWayland cae al rasterizador de
CPU: medido en este equipo con una pagina de 40 tarjetas animadas, el proceso web pasa de
~11% a ~100% de un nucleo. De ahi los tirones al hacer scroll y el consumo alto de CPU.

El renderer DMABUF de WebKit revienta con el driver propietario de NVIDIA en Wayland
(`Error 71 (Error de protocolo) dispatching to Wayland display`), asi que se desactiva con
`WEBKIT_DISABLE_DMABUF_RENDERER=1`; aun asi sigue componiendo por GPU, no se pierde nada.

Vesk fija ambas variables al arrancar, antes de crear la ventana, asi que tambien funciona
lanzado desde `.desktop`. En el shell de Nix, `VESK_GDK_BACKEND=x11` fuerza XWayland (lento
pero estable si tu compositor falla).

### Backend de ventanas

El backend CEF tambien elige el sistema de ventanas al arrancar:

- **Wayland nativo** si la sesion lo ofrece (`WAYLAND_DISPLAY` definido o
  `XDG_SESSION_TYPE=wayland`). CEF crea su propia ventana de nivel superior (CEF Views) y
  GTK usa Wayland para la bandeja, sin pasar por XWayland.
- **X11/XWayland** en cualquier otro caso.

`VESK_GDK_BACKEND=wayland` o `=x11` fuerza uno de los dos. En Wayland nativo el protocolo
no expone posicion global de ventana: `set_size`/`set_position` son no-ops y el estado de
ventana que guarda `tauri-plugin-window-state` no es fiable. Mostrar/ocultar, foco,
maximizar y pantalla completa si funcionan.

### GSettings en NixOS

GTK y WebKit sacan tema, fuentes, escalado y preferencias de GSettings. Si el perfil no tiene
los esquemas, la app arranca con `gsettings_schema_source_lookup: assertion 'source != NULL'
failed` y el contenido sale sin estilos ni imagenes (es el fallo de
[tauri-apps/tauri#14187](https://github.com/tauri-apps/tauri/issues/14187)).

El shell de Nix exporta `XDG_DATA_DIRS` con los esquemas de `gsettings-desktop-schemas` y
`gtk3`, y `GIO_EXTRA_MODULES` con `glib-networking` (TLS para que carguen los recursos
https). Para el binario fuera del shell:

- instala los esquemas en el sistema (`environment.systemPackages = [ pkgs.gsettings-desktop-schemas ];`), o
- envuelve el paquete con `wrapGAppsHook4`, que rellena `XDG_DATA_DIRS` y `GSETTINGS_SCHEMA_DIR`.

## Optimizaciones al estilo OpenAsar

Vesk aplica en el contenedor lo que OpenAsar hace en el proceso principal de Electron:

- **Flags de GPU en CEF**: tres presets en `VESK_GPU_PERF`.
  - `full`: el preset de OpenAsar (zero-copy, `--ignore-gpu-blocklist`, overlays,
    `EnableDrDc`, `CanvasOopRasterization`, sin Vulkan). Es el defecto en GPUs Mesa.
  - `safe` (defecto con el driver propietario de NVIDIA): solo rasterizado por GPU y
    `WebAssemblyLazyCompilation`. Zero-copy y overlays piden GpuMemoryBuffer, que ese
    driver no expone bajo X11: el proceso GPU muere con SIGSEGV/SIGTRAP al rasterizar
    un canvas.
  - `0`: sin flags nuevos.
- **Sin telemetria**: `/api/v*/science` y `/api/v*/metrics` se responden con un 204 antes
  de que Discord registre sus modulos, y Sentry se apaga en cuanto se inicializa.
  `VESK_NO_TRACK=0` lo desactiva.
- **Optimizador de DOM**: retrasa 100 ms el `removeChild` de los nodos de actividad de la
  lista de miembros, que React reescribe constantemente. `VESK_DOM_OPTIMIZER=0` lo
  desactiva.
- **Reposo real**: al ocultarse, `Webview::hide()` avisa a CEF (`was_hidden`) y ya no
  se desactiva `IntensiveWakeUpThrottling` ni el throttling de timers, asi que Chromium
  deja de componer y estrangula a Discord. Medido en este equipo con `/proc`:
  - ventana oculta: **4-5%** de un nucleo (antes 32-45%), con 9 procesos vivos;
  - ventana visible en reposo: **5-10%**;
  - al volver de la bandeja se recuperan los 60 fps (probado por CDP con
    `requestAnimationFrame`), no queda la pagina congelada.
  Al cerrar con la X el ocultado se difiere a un hilo: dentro del callback de
  `CloseRequested` la ventana se queda mapeada y el hilo principal gira.
- **Memoria**: el PSS ronda **1.0 GB** y casi todo es de Discord: renderer ~470 MB,
  navegador ~220 MB, proceso GPU ~120 MB, servicio de red ~110 MB, audio ~40 MB y los
  zygotes ~85 MB. Probado y descartado por no mover la aguja o empeorar la fluidez:
  `enable-low-end-device-mode`, tope de cache en disco, desactivar Vencord, audio en el
  proceso del navegador, `HeapProfiler.collectGarbage` al ocultar (recupera 13 MB) y el
  preset de GPU `0` (ahorra 13 MB pero sube el reposo de 5% a 15% de CPU).
- **Latencia de audio**: `PULSE_LATENCY_MSEC=30` para la voz.
- **Splash ligero**: el arte embebido en el script de inyeccion pasa de 121 KB a 30 KB en
  base64 (copia de 256x256 en `assets/splash.jpg`).
- **Preconexion** a `cdn.discordapp.com`, `media.discordapp.net` y `gateway.discord.gg`
  antes de que Discord los pida.

## Que funciona

- Carga `discord.com/app` con User-Agent de Chrome.
- Splash de carga con el arte del programa (anillo girando + imagen pulsando) que se desvanece
  cuando Discord ya renderizo la app.
- Inyeccion de Vencord (build *userscript*) en el mundo principal, antes de que arranque Discord.
- Shim de `unsafeWindow` y `GM_xmlhttpRequest` para que el userscript corra sin gestor.
- Ajustes reducidos a una seccion **Vesk** informativa: banner con el arte del programa, version
  y limitaciones. Sin pestanas de plugins, temas, Cloud ni Backup (el bundle se parchea al inyectar;
  si Vencord cambia el literal, se avisa por stderr y se usan los ajustes originales).
- Bandeja del sistema: mostrar, recargar, devtools, salir y un submenu de
  **Audio al compartir pantalla** (Sistema menos Vesk, elegir aplicacion o desactivado).
- Compartir pantalla con audio por aplicacion: en lugar del loopback de Chromium (que
  captura **todo** el sistema, incluido el propio cliente de Vesk, y causa eco), Vesk crea
  una fuente virtual de PipeWire `Vesk Screen Share` y enlaza solo el audio elegido.
  El cliente de Vesk siempre se excluye. Se elige el modo desde la bandeja; por defecto es
  **Sistema (menos Vesk)**. Variables: `VESK_SCREEN_AUDIO=0` desactiva el parche,
  `VESK_SCREEN_AUDIO_APP=nombre` fija el modo a una aplicacion concreta al arrancar.
- Cerrar la ventana la oculta en la bandeja (`VESK_CLOSE_TO_TRAY=0` lo desactiva). La
  webview se marca oculta para CEF antes de desmapear la ventana.
- Instancia unica y enlaces `discord://` (tambien al arrancar con un argumento).
- Enlaces externos se abren en el navegador del sistema.
- Persistencia de tamano/posicion de ventana.
- Sesion persistente: copia del token en `session.token` y restauracion al arrancar.

## Requisitos

- Rust 1.82+
- Linux: GTK3, WebKitGTK 4.1, libsoup3, libayatana-appindicator, GStreamer.
- En NixOS, usa el shell incluido.

## Compilar y ejecutar

```bash
nix-shell --run "cargo run -p vesk"
```

Sin Nix, con las dependencias del sistema instaladas:

```bash
cargo run -p vesk
```

Build de release:

```bash
nix-shell --run "cargo build --release -p vesk"
```

## Actualizar Vencord

El bundle vive en `src-tauri/resources/vencord.js` y se embebe en el binario con
`include_str!`. Para actualizarlo:

```bash
./scripts/fetch-vencord.sh
nix-shell --run "cargo build -p vesk"
```

## Enlaces `discord://`

La app acepta `discord://-/channels/...` como argumento y lo traduce a
`https://discord.com/channels/...`. Para registrar el esquema en el escritorio, copia
`packaging/vesk.desktop` a `~/.local/share/applications/` y ejecuta:

```bash
xdg-mime default vesk.desktop x-scheme-handler/discord
```


## Sesion persistente

Vesk lee el token de Discord desde el SQLite de localStorage y lo guarda en
`~/.local/share/dev.vencord.vesk/session.token` con permisos `0600`. Al arrancar, si
localStorage no tiene token, lo reinyecta antes de que Discord ejecute su JavaScript.

- El archivo contiene un token de sesion: tratalo como una contrasena y no lo compartas.
- Para cerrar sesion usa **Cerrar sesion** en la bandeja: borra la copia y navega a
  `discord.com/logout`.
- Si Discord se queda sin token durante un minuto (logout real), la copia se borra sola.
- El vigilante solo abre el SQLite cuando el archivo cambia: mantenerlo abierto bloqueaba
  escrituras de WebKit y provocaba tirones cada pocos segundos.

Probado con un token sintetico: se guarda al vuelo y se restaura tras reiniciar.

## Limitaciones conocidas (importante)

El backend por defecto es **CEF** (Chromium embebido). El backend `wry` usa WebKitGTK.

### CEF (por defecto)

- **Compartir pantalla**: funciona con `getDisplayMedia` y el selector del portal en
  Wayland. El audio se enruta por una fuente virtual de PipeWire (`Vesk Screen Share`)
  creada por Vesk: se puede enviar el sistema completo (menos el propio cliente, para evitar
  eco) o solo una aplicacion concreta. Elige el modo en el submenu **Audio al compartir
  pantalla** de la bandeja. `VESK_SCREEN_AUDIO=0` desactiva el parche.
- **arrpc**: el Rich Presence de juegos y apps locales no esta implementado.
- **Krisp / codecs propietarios**: no disponibles.
- **Notificaciones**: dependen del portal del escritorio; no hay integracion nativa aun.
- Algunos flujos de Discord (pagos, passkeys, captchas) pueden detectar el entorno.

### wry (WebKitGTK)

- **Voz**: WebRTC de WebKitGTK puede funcionar, pero la cadena de audio de Discord no esta
  probada y `getUserMedia` puede requerir permisos que wry no expone todavia.
- **Compartir pantalla**: WebKitGTK no implementa `getDisplayMedia`. No funciona.
- **venmic / arrpc**: no existen en esta version.
- **Notificaciones**: dependen del portal del escritorio.
- **Krisp / codecs propietarios**: no disponibles.
- Algunos flujos de Discord (pagos, passkeys, captchas) pueden detectar WebKit y fallar.

## Estructura

```
Cargo.toml                  workspace
src-tauri/
  Cargo.toml
  tauri.conf.json
  capabilities/default.json
  src/main.rs               ventana, bandeja, deep links, inyeccion
  src/session.rs            vault del token de sesion
  resources/vencord.js      bundle de Vencord (GPL-3.0)
ui/index.html               fallback local (no se usa para Discord)
assets/icon-source.jpeg     imagen original del icono
scripts/fetch-vencord.sh
packaging/vesk.desktop
shell.nix / flake.nix
```

## Licencia

GPL-3.0-or-later, igual que Vesktop y Vencord. El bundle de Vencord conserva su licencia y su
aviso legal en `src-tauri/resources/vencord.js.LEGAL.txt`.
