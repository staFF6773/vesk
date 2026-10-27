<p align="center">
  <img src="src-tauri/icons/128x128@2x.png" alt="Logo de Vesk" width="144">
</p>

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
traduccion de `discord://`, el filtro de navegacion, el arte embebido, los flags de CEF y
el protocolo IPC/bridge de Rich Presence.

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

### CEF y codecs

`nix/cef-codecs.nix` extrae el paquete oficial de Arch `cef 150.0.14-1` (mismo commit
`7c1aa68` y Chromium `150.0.7871.129` que el tarball de CEF). A diferencia del tarball
oficial, viene compilado con `proprietary_codecs=true`, asi que reproduce los MP4
H.264/AAC de Discord. Usa `libjpeg8` de nixpkgs para el ABI `libjpeg.so.8` y empaqueta
`libopenh264.so.8` de Arch; `codec_probe` de `tauri-runtime-cef` confirma que tanto el
MP4 H.264/AAC como el WebM VP9/Opus se reproducen.

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
- **Reproduccion eficiente de medios**: pausa videos y GIFs que salen del viewport y, al
  ocultar la ventana, solo los mudos o en bucle (el audio audible sigue). No toca los
  videos de llamadas ni de compartir pantalla (`srcObject`). Los GIFs pausados reinician
  su animacion al volver. `VESK_MEDIA_OPTIMIZER=0` lo desactiva.
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
- Reproduce GIFs y videos MP4 H.264/AAC: el CEF de Arch incluye los codecs
  propietarios que Discord usa para los adjuntos.
- Splash de carga con el arte del programa (anillo girando + imagen pulsando) que se desvanece
  cuando Discord ya renderizo la app.
- Inyeccion de Vencord (build *userscript*) en el mundo principal, antes de que arranque Discord.
- Shim de `unsafeWindow` y `GM_xmlhttpRequest` para que el userscript corra sin gestor.
- **Rich Presence (arRPC)**: los juegos que hablan con el IPC local de Discord aparecen en el
  perfil, sin Node ni un servidor externo. Incluye un bridge WebSocket en `127.0.0.1:1337`
  para el plugin `WebRichPresence (arRPC)` de Vencord y escaneo de procesos Linux.
- Ajustes reducidos a una seccion **Vesk** informativa: banner con el arte del programa, version
  y limitaciones. Sin pestanas de plugins, temas, Cloud ni Backup (el bundle se parchea al inyectar;
  si Vencord cambia el literal, se avisa por stderr y se usan los ajustes originales).
- Bandeja del sistema: mostrar, recargar, devtools, salir, un submenu de
  **Rich Presence** (activar o desactivar la actividad de juegos) y otro de
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

## Rich Presence (arRPC)

Vesk implementa el servidor local de arRPC en Rust. No necesita Node ni ejecutar `npx arrpc`:

- Escucha el socket IPC de Discord (`$XDG_RUNTIME_DIR/discord-ipc-0..9`) para que los juegos
  con Discord RPC envien su actividad. Si el Discord de escritorio real ya ocupa el socket,
  Vesk usa el siguiente libre.
- Expone el bridge WebSocket `ws://127.0.0.1:1337`, que es el que consume el plugin
  `WebRichPresence (arRPC)` incluido en Vencord. El plugin no se habilita en los ajustes:
  Vesk llama directamente a su `handleEvent`, como hace Vesktop.
- Escanea `/proc` cada 5 segundos con una copia recortada de la tabla Linux de
  [OpenAsar/arRPC](https://github.com/OpenAsar/arRPC) (MIT, commit `3c4e1ce`). Esa tabla
  solo incluye unos pocos juegos Linux; la mayoria se detecta por IPC.
- Se activa al arrancar. Se puede apagar desde **Rich Presence → Actividad de juegos** en la
  bandeja o con `VESK_ARRPC=0`. `VESK_ARRPC_SCAN=0` desactiva solo el escaneo de procesos.
- Si el puerto `1337` ya esta ocupado por otro arRPC, Vesk no levanta un servidor duplicado
  y deja que el cliente use la instancia externa.
- Las invitaciones y deep links de arRPC no se reenvian; Vesk ya gestiona los enlaces
  `discord://` por separado.

## Requisitos

- Rust 1.88+ (edicion 2021; el workspace fija `rust-version`)
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
- **Krisp**: no disponible. H.264 y AAC si estan incluidos en el CEF de Arch.
- **Notificaciones**: dependen del portal del escritorio; no hay integracion nativa aun.
- Algunos flujos de Discord (pagos, passkeys, captchas) pueden detectar el entorno.

### wry (WebKitGTK)

- **Voz**: WebRTC de WebKitGTK puede funcionar, pero la cadena de audio de Discord no esta
  probada y `getUserMedia` puede requerir permisos que wry no expone todavia.
- **Compartir pantalla**: WebKitGTK no implementa `getDisplayMedia`. No funciona.
- **venmic**: no existe en esta version.
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
  src/arrpc.rs              IPC de Discord, bridge WebSocket y escaneo
  src/session.rs            vault del token de sesion
  resources/arrpc-linux.json tabla Linux de arRPC (MIT)
  resources/arrpc-linux.LICENSE licencia de esa tabla
  resources/vencord.js      bundle de Vencord (GPL-3.0)
ui/index.html               fallback local (no se usa para Discord)
assets/icon-source.jpeg     imagen original del icono
scripts/fetch-vencord.sh
packaging/vesk.desktop
nix/cef-codecs.nix          CEF 150 de Arch con H.264/AAC
shell.nix / flake.nix
```

## Licencia

GPL-3.0-or-later, igual que Vesktop y Vencord. El bundle de Vencord conserva su licencia y su
aviso legal en `src-tauri/resources/vencord.js.LEGAL.txt`. La tabla Linux de arRPC derivada de
OpenAsar/arRPC mantiene su licencia MIT en `src-tauri/resources/arrpc-linux.LICENSE`.
