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
traduccion de `discord://` y el filtro de navegacion.

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

## Que funciona

- Carga `discord.com/app` con User-Agent de Chrome.
- Splash de carga con el arte del programa (anillo girando + imagen pulsando) que se desvanece
  cuando Discord ya renderizo la app.
- Inyeccion de Vencord (build *userscript*) en el mundo principal, antes de que arranque Discord.
- Shim de `unsafeWindow` y `GM_xmlhttpRequest` para que el userscript corra sin gestor.
- Ajustes reducidos a una seccion **Vesk** informativa: banner con el arte del programa, version
  y limitaciones. Sin pestanas de plugins, temas, Cloud ni Backup (el bundle se parchea al inyectar;
  si Vencord cambia el literal, se avisa por stderr y se usan los ajustes originales).
- Bandeja del sistema: mostrar, recargar, devtools, salir.
- Cerrar la ventana la oculta en la bandeja (`VESK_CLOSE_TO_TRAY=0` lo desactiva).
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

Tauri usa el webview del sistema, no Chromium. En Linux eso significa WebKitGTK:

- **Voz**: WebRTC de WebKitGTK puede funcionar, pero la cadena de audio de Discord no esta
  probada y `getUserMedia` puede requerir permisos que wry no expone todavia.
- **Compartir pantalla**: WebKitGTK no implementa `getDisplayMedia` de forma equiparable a
  Chromium. No funciona.
- **venmic / arrpc**: los modulos nativos de Vesktop para audio de pantalla y Rich Presence
  no existen en esta version.
- **Notificaciones**: dependen del portal del escritorio; no hay integracion nativa aun.
- **Krisp / codecs propietarios**: no disponibles.
- Algunos flujos de Discord (pagos, passkeys, captchas) pueden detectar WebKit y fallar.

Para paridad real con Vesktop hace falta un backend **CEF** (Chromium embebido) en lugar del
webview del sistema. La estructura actual permite anadir ese backend sin tocar la capa de
inyeccion de Vencord.

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
