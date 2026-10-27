#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

#[cfg(all(feature = "cef", target_os = "linux"))]
mod cef;

mod arrpc;
mod screen_audio;
mod session;

use screen_audio::{AppInfo, Mode};
use session::SessionVault;

#[tauri::command]
fn screen_audio_set(mode: String) -> Result<(), String> {
    screen_audio::set_mode(parse_mode(&mode)?);
    Ok(())
}

#[tauri::command]
fn screen_audio_list() -> Vec<AppInfo> {
    screen_audio::list_apps()
}

fn parse_mode(value: &str) -> Result<Mode, String> {
    match value {
        "off" => Ok(Mode::Off),
        "system" => Ok(Mode::System),
        other => {
            let app = other.strip_prefix("app:").unwrap_or(other);
            if app.is_empty() {
                return Err("modo de audio invalido".to_owned());
            }
            Ok(Mode::App(app.to_owned()))
        }
    }
}

#[cfg(all(feature = "cef", target_os = "linux"))]
type BrowserEngine = tauri_runtime_cef::CefRuntime<tauri::EventLoopMessage>;
#[cfg(not(all(feature = "cef", target_os = "linux")))]
type BrowserEngine = tauri::Wry;

type App = tauri::App<BrowserEngine>;
type AppHandle = tauri::AppHandle<BrowserEngine>;
type WebviewWindow = tauri::WebviewWindow<BrowserEngine>;

const DISCORD_URL: &str = "https://discord.com/app";

/// Discord sirve cosas distintas segun el User-Agent. Electron se presenta como
/// Chrome; WebKitGTK se presenta como Safari, asi que lo suplantamos. CEF ya es
/// Chromium y usa su propio User-Agent.
#[cfg(not(all(feature = "cef", target_os = "linux")))]
const CHROME_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

/// Build "userscript" de Vencord, inyectado al inicio del documento.
/// Se actualiza con scripts/fetch-vencord.sh.
const VENCORD_BUNDLE: &str = include_str!("../resources/vencord.js");

/// Entradas que Vencord registra en los ajustes de Discord (Vencord, Plugins,
/// Themes, Cloud, Backup & Restore y las que anaden los plugins).
const VENCORD_SETTINGS_ENTRIES: &str = r#"o({key:"vencord_main",title:"Vencord",panelTitle:"Vencord Settings",Component:Jd,Icon:Jh}),o({key:"vencord_plugins",title:"Plugins",Component:wa,Icon:Xh}),o({key:"vencord_themes",title:"Themes",Component:hs,Icon:Hs}),!1,o({key:"vencord_cloud",title:"Cloud",panelTitle:"Vencord Cloud",Component:Kd,Icon:Vh}),o({key:"vencord_backup_restore",title:"Backup & Restore",Component:$d,Icon:e1}),!1,...this.customEntries.map(o),...this.customSections.map((p,h)=>{let{section:y,element:b,label:w}=p(Gx);return Object.values(Gx).includes(y)?null:o({key:`vencord_deprecated_custom_${y}`,title:w,Component:b,Icon:y==="Vesktop"?t1:bl})})"#;

/// Vesk solo muestra informacion: no hay pestanas de plugins ni de temas.
const VESK_SETTINGS_ENTRY: &str = r#"o({key:"vesk_main",title:"Vesk",panelTitle:"Acerca de Vesk",Component:window.__VESK_ABOUT__,Icon:Jh})"#;

/// Arte original del programa, embebido para el splash y el banner de los
/// ajustes. Es una copia de 256x256: el original pesa 90 KB y solo se pinta a
/// 104 px, asi que en base64 anadia ~120 KB al script de inyeccion.
const VESK_ART: &[u8] = include_bytes!("../../assets/splash.jpg");

/// Repositorio publico del proyecto, enlazado desde la pagina de ajustes.
const VESK_REPO: &str = "https://github.com/staFF6773/vesk";

/// Codifica el arte como data URI sin anadir dependencias. Se cachea porque el
/// script de inyeccion lo pide en cada arranque.
fn vesk_art_data_uri() -> &'static str {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE
        .get_or_init(|| data_uri("image/jpeg", VESK_ART))
        .as_str()
}

fn data_uri(mime: &str, bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity(mime.len() + 15 + bytes.len().div_ceil(3) * 4);
    out.push_str("data:");
    out.push_str(mime);
    out.push_str(";base64,");

    for chunk in bytes.chunks(3) {
        let n = ((chunk[0] as u32) << 16)
            | ((*chunk.get(1).unwrap_or(&0) as u32) << 8)
            | *chunk.get(2).unwrap_or(&0) as u32;

        out.push(ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(n >> 12) as usize & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 0x3f] as char
        } else {
            '='
        });
    }

    out
}

/// Interruptor de entorno: 0/false/no/off lo apaga, ausente usa el valor por
/// defecto.
pub(crate) fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => default,
    }
}

static TRAY_HINTED: AtomicBool = AtomicBool::new(false);

/// Entorno que GTK y WebKit leen al arrancar. Lanzado desde el escritorio no
/// hay shell que lo prepare, asi que lo fijamos antes de crear la ventana.
#[cfg(not(all(feature = "cef", target_os = "linux")))]
fn configure_webkit_env() {
    // Bajo XWayland WebKit pierde la composicion por GPU y rasteriza en CPU:
    // medido 100% de un nucleo frente a 11% en Wayland nativo.
    if std::env::var_os("GDK_BACKEND").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        std::env::set_var("GDK_BACKEND", "wayland");
    }

    // El renderer DMABUF de WebKit rompe con el driver propietario de NVIDIA
    // ("Error 71 de protocolo" en Wayland). Desactivado sigue componiendo por
    // GPU, asi que no cuesta rendimiento.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none()
        && std::path::Path::new("/dev/nvidiactl").exists()
    {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}

fn main() {
    #[cfg(all(feature = "cef", target_os = "linux"))]
    {
        cef::configure_env();
        cef::configure();

        if std::env::args().any(|arg| arg.starts_with("--type=")) {
            tauri_runtime_cef::run_cef_helper_process();
            return;
        }

        cef::install_permission_policy();
        cef::install_popup_policy();
    }
    #[cfg(not(all(feature = "cef", target_os = "linux")))]
    configure_webkit_env();

    tauri::Builder::<BrowserEngine>::new()
        .invoke_handler(tauri::generate_handler![screen_audio_set, screen_audio_list])
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if let Some(link) = argv.iter().find(|arg| arg.starts_with("discord://")) {
                navigate_deep_link(app, link);
            }
            reveal(app);
        }))
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .setup(|app| {
            #[cfg(all(feature = "cef", target_os = "linux"))]
            {
                gtk::init()?;
                tauri_runtime_cef::install_x_error_handlers();
            }

            let initial = std::env::args()
                .skip(1)
                .find_map(|arg| discord_deep_link_to_https(&arg))
                .unwrap_or_else(|| DISCORD_URL.to_owned());

            let data_dir = app.path().app_local_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let vault = SessionVault::new(data_dir);
            #[cfg(not(all(feature = "cef", target_os = "linux")))]
            vault.watch();

            arrpc::init();
            let window = create_main_window(app, &initial, Some(&vault))?;
            install_close_to_tray(&window);
            if let Ok(raw) = std::env::var("VESK_TRAY_CYCLE") {
                let delay: u64 = raw.parse().unwrap_or(15000);
                let hold: u64 = std::env::var("VESK_TRAY_HOLD")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(6);
                let win = window.clone();
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                    hide_to_tray(&win);
                    std::thread::sleep(std::time::Duration::from_secs(hold));
                    reveal(&handle);
                });
            }
            app.manage(vault);

            screen_audio::init();
            create_tray(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("no se pudo iniciar Vesk");

    screen_audio::shutdown();
    arrpc::shutdown();
}

fn create_main_window(
    app: &App,
    initial: &str,
    vault: Option<&SessionVault>,
) -> tauri::Result<WebviewWindow> {
    let url = url::Url::parse(initial).unwrap_or_else(|_| url::Url::parse(DISCORD_URL).unwrap());
    let script = injection_script(vault);

    let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
        .title("Vesk")
        .inner_size(1280.0, 800.0)
        .min_inner_size(720.0, 480.0)
        // Mismo fondo que el splash para que no haya destello blanco al abrir.
        .background_color(tauri::window::Color(30, 31, 34, 255))
        .initialization_script(&script)
        .on_navigation(|url| {
            if is_discord_url(url.as_str()) {
                return true;
            }

            // Enlaces externos (Stripe, soporte, etc.) al navegador del sistema.
            let target = url.to_string();
            std::thread::spawn(move || {
                let _ = open::that(target);
            });
            false
        });

    #[cfg(not(all(feature = "cef", target_os = "linux")))]
    let builder = builder.user_agent(CHROME_UA);

    Ok(builder.build()?)
}

/// Reduce los ajustes de Vencord a una unica pagina informativa de Vesk.
///
/// El bundle llega minificado, asi que sustituimos el literal exacto que arma
/// la seccion. Si una actualizacion de Vencord cambia ese literal, devolvemos el
/// bundle intacto y avisamos por stderr: los ajustes completos volverian a
/// aparecer, pero la app sigue funcionando.
fn vencord_bundle() -> String {
    let patched = VENCORD_BUNDLE
        .replace(VENCORD_SETTINGS_ENTRIES, VESK_SETTINGS_ENTRY)
        .replace(
            r#"useTitle:()=>"Vencord Settings",buildLayout:()=>n"#,
            r#"useTitle:()=>"Vesk",buildLayout:()=>n"#,
        );

    if !patched.contains(VESK_SETTINGS_ENTRY) {
        eprintln!(
            "[vesk] el bundle de Vencord cambio: no se pudieron reducir los ajustes a la pagina de Vesk"
        );
    }

    patched
}

/// Vencord espera el mundo principal y dos APIs de userscript. Al inyectar con
/// initialization_script ya estamos en el mundo principal del documento.
fn injection_script(vault: Option<&SessionVault>) -> String {
    let mut prefix = String::new();

    if let Some(vault) = vault {
        if let Some(token) = vault.load() {
            let quoted = serde_json::to_string(&token).unwrap_or_else(|_| "\"\"".to_owned());
            prefix.push_str(&format!(
                "try {{ if (!localStorage.getItem('token')) localStorage.setItem('token', {quoted}); }} catch (e) {{}}\n"
            ));
        }
    }

    const BRIDGE: &str = r##"
(() => {
  if (typeof window.unsafeWindow === "undefined") window.unsafeWindow = window;

  if (typeof window.GM_xmlhttpRequest === "undefined") {
    window.GM_xmlhttpRequest = (details) => {
      const controller = new AbortController();
      const init = {
        method: details.method || "GET",
        headers: details.headers || {},
        signal: controller.signal,
      };
      if (details.data !== undefined) init.body = details.data;

      fetch(details.url, init)
        .then(async (response) => {
          const responseText = await response.text();
          if (typeof details.onload === "function") {
            details.onload({
              status: response.status,
              statusText: response.statusText,
              responseText,
              responseHeaders: "",
              finalUrl: response.url,
            });
          }
        })
        .catch((error) => {
          if (typeof details.onerror === "function") details.onerror(error);
        });

      return { abort: () => controller.abort() };
    };
  }

  // --- Audio del sistema por aplicacion al compartir pantalla ---
  // Vesk crea una fuente virtual de PipeWire ("Vesk Screen Share") y enlaza solo
  // el audio elegido, excluyendo siempre el propio cliente para evitar eco.
  // Aqui redirigimos getDisplayMedia a ese microfono virtual.
  if (navigator.mediaDevices && typeof navigator.mediaDevices.getDisplayMedia === "function") {
    const media = navigator.mediaDevices;
    const originalDisplay = media.getDisplayMedia.bind(media);
    const VIRTUAL_LABEL = "Vesk Screen Share";

    const findVirtual = async () => {
      const devices = await media.enumerateDevices();
      const found = devices.find((d) => d.kind === "audioinput" && d.label === VIRTUAL_LABEL);
      if (found) return found.deviceId;
      // Sin permiso aun, los labels estan vacios: pedir microfono para poblarlos.
      try {
        const probe = await media.getUserMedia({ audio: true });
        probe.getTracks().forEach((t) => t.stop());
      } catch (_) {}
      const again = await media.enumerateDevices();
      const retry = again.find((d) => d.kind === "audioinput" && d.label === VIRTUAL_LABEL);
      return retry ? retry.deviceId : null;
    };

    media.getDisplayMedia = async function (constraints) {
      const next = constraints && typeof constraints === "object" ? { ...constraints } : {};
      if (next.video === undefined) next.video = true;

      const deviceId = await findVirtual();
      if (!deviceId) {
        // No hay fuente virtual (audio desactivado o sin PipeWire): solo video.
        delete next.audio;
        return originalDisplay(next);
      }

      next.audio = {
        ...(next.audio && typeof next.audio === "object" ? next.audio : {}),
        deviceId: { exact: deviceId },
        autoGainControl: false,
        echoCancellation: false,
        noiseSuppression: false,
        channelCount: 2,
        sampleRate: 48000,
      };

      try {
        return await originalDisplay(next);
      } catch (error) {
        // Sin servidor de audio el nodo virtual no existe: compartir solo video.
        if (error && (error.name === "NotSupportedError" || error.name === "OverconstrainedError")) {
          const fallback = { ...next };
          delete fallback.audio;
          return originalDisplay(fallback);
        }
        throw error;
      }
    };
  }

  // --- Parches de arranque al estilo OpenAsar ---
  // Se instalan antes que el bundle de Discord, que es cuando sirven: la
  // telemetria sale en los primeros segundos y Sentry se inicializa al cargar
  // el cliente.
  const NO_TRACK = __VESK_NO_TRACK__;
  const DOM_OPTIMIZER = __VESK_DOM_OPTIMIZER__;

  if (NO_TRACK) {
    // /api/v9/science y /api/v9/metrics son solo telemetria. OpenAsar cancela
    // la peticion; devolver un 204 evita ademas que Discord registre un error
    // de red en la consola.
    const TRACKED = /\/api\/v\d+\/(?:science|metrics)(?:[/?]|$)/;

    const nativeFetch = window.fetch;
    if (typeof nativeFetch === "function") {
      window.fetch = function (input, init) {
        try {
          const url = typeof input === "string" ? input : (input && input.url) || "";
          if (TRACKED.test(url)) {
            return Promise.resolve(new Response(null, { status: 204, statusText: "No Content" }));
          }
        } catch (e) {}
        return nativeFetch.apply(this, arguments);
      };
    }

    const nativeBeacon = navigator.sendBeacon;
    if (typeof nativeBeacon === "function") {
      navigator.sendBeacon = function (url) {
        if (TRACKED.test(String(url))) return true;
        return nativeBeacon.apply(this, arguments);
      };
    }
  }

  // Sentry: Discord lo inicializa desde su bundle. Esperamos a que exista y lo
  // apagamos, restaurando los console.* que instrumenta.
  if (NO_TRACK) {
    let attempts = 0;
    const timer = window.setInterval(() => {
      attempts++;
      try {
        const hub = window.__SENTRY__ && window.__SENTRY__.hub;
        const client = hub && hub.getClient && hub.getClient();
        const options = client && client.getOptions && client.getOptions();
        if (options) options.enabled = false;
        Object.keys(console).forEach((key) => {
          const original = console[key] && console[key].__sentry_original__;
          if (original) console[key] = original;
        });
      } catch (e) {}
      if (attempts >= 20) window.clearInterval(timer);
    }, 1000);
  }

  // Optimizador de DOM: la lista de miembros reescribe las actividades cada
  // pocos segundos y React desmonta y vuelve a montar nodos. Retrasar 100 ms el
  // removeChild de esos nodos evita el parpadeo y el coste de layout (el mismo
  // truco que OpenAsar).
  if (DOM_OPTIMIZER && typeof Element !== "undefined") {
    const nativeRemoveChild = Element.prototype.removeChild;
    Element.prototype.removeChild = function (child) {
      if (
        child &&
        typeof child.className === "string" &&
        child.className.indexOf("activity") !== -1
      ) {
        const parent = this;
        const args = arguments;
        window.setTimeout(() => {
          try {
            nativeRemoveChild.apply(parent, args);
          } catch (e) {}
        }, 100);
        return child;
      }
      return nativeRemoveChild.apply(this, arguments);
    };
  }

  // Reproduccion eficiente de videos y GIFs: pausa lo que esta fuera de
  // pantalla y lo que deja de verse al ocultar la ventana. No toca los videos
  // de llamadas o compartir pantalla (srcObject) ni corta el audio audible.
  const MEDIA_OPTIMIZER = __VESK_MEDIA_OPTIMIZER__;

  if (MEDIA_OPTIMIZER) {
    const GIF_PLACEHOLDER =
      "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
    const MARGIN = 300;
    const autoPaused = new WeakSet();
    const gifState = new WeakMap();
    const observed = new WeakSet();

    const isVideo = (el) => !!el && el.tagName === "VIDEO";
    const isImage = (el) => !!el && el.tagName === "IMG";
    const isCallVideo = (el) => isVideo(el) && !!el.srcObject;
    const isSilent = (el) => el.muted || el.volume === 0 || el.hasAttribute("loop");
    const isGif = (el) => {
      if (!isImage(el)) return false;
      const raw = el.currentSrc || el.src || "";
      if (!raw || raw.indexOf("data:") === 0) return false;
      try {
        return new URL(raw, location.href).pathname.toLowerCase().endsWith(".gif");
      } catch (e) {
        return false;
      }
    };
    const isMedia = (el) => isVideo(el) || isGif(el);
    const onScreen = (el) => {
      if (!el.isConnected) return false;
      const rect = el.getBoundingClientRect();
      return (
        rect.bottom > -MARGIN &&
        rect.top < window.innerHeight + MARGIN &&
        rect.right > 0 &&
        rect.left < window.innerWidth
      );
    };

    const pauseVideo = (el, onlySilent) => {
      if (isCallVideo(el) || el.paused || el.ended) return;
      if (onlySilent && !isSilent(el)) return;
      autoPaused.add(el);
      el.pause();
    };

    const pauseGif = (el) => {
      if (!isImage(el) || gifState.has(el)) return;
      const src = el.getAttribute("src");
      const srcset = el.hasAttribute("srcset") ? el.getAttribute("srcset") : null;
      gifState.set(el, { src, srcset });
      el.removeAttribute("srcset");
      el.setAttribute("src", GIF_PLACEHOLDER);
    };

    const resume = (el) => {
      if (isVideo(el)) {
        if (!autoPaused.has(el)) return;
        autoPaused.delete(el);
        if (el.isConnected) {
          const playing = el.play();
          if (playing && typeof playing.catch === "function") playing.catch(() => {});
        }
        return;
      }

      const state = gifState.get(el);
      if (!state) return;
      gifState.delete(el);
      // React pudo reemplazar la imagen mientras estaba pausada: no pisar el src nuevo.
      if (el.getAttribute("src") !== GIF_PLACEHOLDER) return;
      if (state.src === null) el.removeAttribute("src");
      else el.setAttribute("src", state.src);
      if (state.srcset === null) el.removeAttribute("srcset");
      else el.setAttribute("srcset", state.srcset);
    };

    const pauseOffscreen = (el) => {
      if (isVideo(el)) pauseVideo(el, false);
      else if (isGif(el) || gifState.has(el)) pauseGif(el);
    };

    const io = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) {
            if (!document.hidden) resume(entry.target);
          } else if (!document.hidden) {
            pauseOffscreen(entry.target);
          }
        }
      },
      { rootMargin: MARGIN + "px 0px" }
    );

    const track = (root) => {
      if (!root) return;
      if (root.nodeType === 1 && isMedia(root) && !observed.has(root)) {
        observed.add(root);
        io.observe(root);
      }
      if (!root.querySelectorAll) return;
      root.querySelectorAll("video, img").forEach((el) => {
        if (!isMedia(el) || observed.has(el)) return;
        observed.add(el);
        io.observe(el);
      });
    };

    track(document);

    const observer = new MutationObserver((records) => {
      for (const record of records) {
        record.addedNodes.forEach((node) => track(node));
        record.removedNodes.forEach((node) => {
          if (node.nodeType !== 1) return;
          if (isMedia(node)) io.unobserve(node);
          if (node.querySelectorAll) {
            node.querySelectorAll("video, img").forEach((el) => io.unobserve(el));
          }
        });
      }
    });
    observer.observe(document, { childList: true, subtree: true });

    document.addEventListener("visibilitychange", () => {
      document.querySelectorAll("video").forEach((el) => {
        if (document.hidden) pauseVideo(el, true);
        else if (onScreen(el)) resume(el);
      });

      document.querySelectorAll("img").forEach((el) => {
        if (document.hidden) {
          if (isGif(el) || gifState.has(el)) pauseGif(el);
        } else if (gifState.has(el) && onScreen(el)) {
          resume(el);
        }
      });
    });
  }

  // Preconexion a los origenes que Discord usa despues del documento inicial.
  (() => {
    const root = document.head || document.documentElement;
    if (!root) return;
    [
      "https://cdn.discordapp.com",
      "https://media.discordapp.net",
      "https://gateway.discord.gg",
    ].forEach((href) => {
      const link = document.createElement("link");
      link.rel = "preconnect";
      link.href = href;
      link.crossOrigin = "anonymous";
      root.appendChild(link);
    });
  })();

  window.__VESK__ = {
    shell: "tauri",
    engine: "__VESK_ENGINE__",
    window: "__VESK_WINDOWING__",
    version: "__VESK_VERSION__",
    art: "__VESK_ICON__",
    repo: "__VESK_REPO__",
    arrpc: __VESK_ARRPC__,
  };

  console.log(
    "[vesk] motor=" + window.__VESK__.engine +
    " RTCPeerConnection=" + typeof RTCPeerConnection +
    " mediaDevices=" + !!navigator.mediaDevices +
    " getUserMedia=" + typeof (navigator.mediaDevices && navigator.mediaDevices.getUserMedia) +
    " getDisplayMedia=" + typeof (navigator.mediaDevices && navigator.mediaDevices.getDisplayMedia)
  );

  // --- Rich Presence (arRPC) ---
  // El servidor de Rust escucha en 127.0.0.1:1337 y el plugin de Vencord
  // convierte cada mensaje en LOCAL_ACTIVITY_UPDATE. No habilitamos el plugin:
  // llamamos a handleEvent directamente para no tocar sus ajustes ni mostrar
  // el aviso de "conectado".
  (() => {
    if (!window.__VESK__) return;

    const PLUGIN = "WebRichPresence (arRPC)";
    let socket = null;
    let enabled = !!window.__VESK__.arrpc;
    let retry = null;
    let waiting = null;
    let warned = false;

    const plugin = () => {
      const vencord = window.Vencord;
      return (
        vencord &&
        vencord.Plugins &&
        vencord.Plugins.plugins &&
        vencord.Plugins.plugins[PLUGIN]
      );
    };

    const schedule = () => {
      if (!enabled || retry || socket) return;
      retry = setTimeout(() => {
        retry = null;
        connect();
      }, 3000);
    };

    const waitForPlugin = () => {
      if (!enabled || waiting) return;
      waiting = setInterval(() => {
        if (!enabled || !plugin()) return;
        clearInterval(waiting);
        waiting = null;
        connect();
      }, 250);
    };

    const connect = () => {
      if (!enabled || socket) return;

      const arRPC = plugin();
      if (!arRPC || typeof arRPC.handleEvent !== "function") {
        if (!warned) {
          warned = true;
          console.warn("[vesk] el plugin WebRichPresence (arRPC) no esta disponible");
        }
        schedule();
        return;
      }

      try {
        socket = new WebSocket("ws://127.0.0.1:1337");
      } catch (error) {
        schedule();
        return;
      }

      socket.onmessage = (event) => {
        try {
          arRPC.handleEvent(event);
        } catch (error) {
          console.error("[vesk] no se pudo aplicar la actividad Rich Presence", error);
        }
      };
      socket.onclose = () => {
        socket = null;
        schedule();
      };
      socket.onerror = () => {
        try {
          if (socket) socket.close();
        } catch (_) {}
      };
    };

    window.__VESK_RICH_PRESENCE__ = {
      setEnabled(value) {
        enabled = !!value;
        if (retry) {
          clearTimeout(retry);
          retry = null;
        }
        if (waiting) {
          clearInterval(waiting);
          waiting = null;
        }
        if (!enabled && socket) {
          socket.onmessage = null;
          socket.onclose = null;
          socket.onerror = null;
          try {
            socket.close();
          } catch (_) {}
          socket = null;
        }
        if (!enabled) {
          const arRPC = plugin();
          if (arRPC && typeof arRPC.handleEvent === "function") {
            try {
              arRPC.handleEvent({ data: JSON.stringify({ activity: null }) });
            } catch (_) {}
          }
        }
        if (enabled) waitForPlugin();
      },
    };

    // El bundle de Vencord se evalua despues de este script, y el interruptor
    // de la bandeja puede activar esto cuando el arranque estaba desactivado.
    if (enabled) waitForPlugin();
  })();

  // Pagina "Acerca de Vesk" que sustituye a los ajustes de Vencord.
  // React se resuelve al renderizar, cuando Vencord ya expuso su API.
  window.__VESK_ABOUT__ = function VeskAbout() {
    const webpack = window.Vencord && window.Vencord.Webpack;
    const React = webpack && webpack.Common && webpack.Common.React;
    if (!React) return null;

    const h = React.createElement;
    const version = (window.__VESK__ && window.__VESK__.version) || "";
    const art = (window.__VESK__ && window.__VESK__.art) || "";
    const engine = (window.__VESK__ && window.__VESK__.engine) || "webkit";
    const windowing = (window.__VESK__ && window.__VESK__.window) || "x11";
    const repo =
      (window.__VESK__ && window.__VESK__.repo) ||
      "https://github.com/staFF6773/vesk";
    const cardStyle = {
      padding: "16px 18px",
      borderRadius: 8,
      border:
        "1px solid var(--background-modifier-accent, rgba(255,255,255,.06))",
      background: "var(--background-secondary, #2b2d31)",
    };
    const titleStyle = {
      color: "var(--header-primary, #f2f3f5)",
      fontSize: 12,
      fontWeight: 700,
      letterSpacing: ".02em",
      textTransform: "uppercase",
      margin: "0 0 10px",
    };
    const pillStyle = {
      display: "inline-flex",
      alignItems: "center",
      padding: "2px 8px",
      borderRadius: 999,
      fontSize: 12,
      fontWeight: 600,
      color: "var(--text-normal, #dbdee1)",
      background:
        "var(--background-modifier-selected, rgba(255,255,255,.08))",
      border:
        "1px solid var(--background-modifier-accent, rgba(255,255,255,.06))",
    };
    const linkStyle = {
      color: "var(--text-link, #00a8fc)",
      textDecoration: "none",
      fontWeight: 600,
    };
    const buttonStyle = {
      display: "inline-flex",
      alignItems: "center",
      justifyContent: "center",
      padding: "8px 14px",
      borderRadius: 6,
      background: "var(--brand-500, #5865f2)",
      color: "#fff",
      textDecoration: "none",
      fontWeight: 600,
      fontSize: 14,
    };
    const secondaryButtonStyle = {
      ...buttonStyle,
      color: "var(--text-normal, #dbdee1)",
      background:
        "var(--background-modifier-selected, rgba(255,255,255,.08))",
      border:
        "1px solid var(--background-modifier-accent, rgba(255,255,255,.06))",
    };
    const item = (text) =>
      h("li", { key: text, style: { marginBottom: 6 } }, text);
    const group = (title, items) =>
      h(
        "section",
        { style: cardStyle },
        h("h5", { style: titleStyle }, title),
        h("ul", { style: { margin: 0, paddingLeft: 20 } }, items.map(item))
      );

    // Banner con el arte del programa, version, motor y enlace al repositorio.
    const banner = h(
      "div",
      {
        style: {
          display: "flex",
          alignItems: "center",
          gap: 24,
          padding: 24,
          borderRadius: 8,
          overflow: "hidden",
          border:
            "1px solid var(--background-modifier-accent, rgba(255,255,255,.06))",
          background:
            "linear-gradient(135deg, var(--background-secondary, #2b2d31) 0%, var(--background-tertiary, #1e1f22) 100%)",
          flexWrap: "wrap",
        },
      },
      h(
        "div",
        { style: { flex: "1 1 320px", minWidth: 0 } },
        h(
          "div",
          {
            style: {
              display: "flex",
              alignItems: "center",
              gap: 10,
              flexWrap: "wrap",
              marginBottom: 8,
            },
          },
          h(
            "h2",
            {
              style: {
                color: "var(--header-primary, #f2f3f5)",
                margin: 0,
                fontSize: 26,
              },
            },
            "Vesk"
          ),
          version ? h("span", { style: pillStyle }, "v" + version) : null
        ),
        h(
          "p",
          {
            style: {
              color: "var(--text-muted, #949ba4)",
              margin: "0 0 14px",
            },
          },
          "Shell de Discord en Rust + Tauri 2 con Vencord inyectado en el webview del sistema."
        ),
        h(
          "div",
          { style: { display: "flex", gap: 8, flexWrap: "wrap" } },
          h("span", { style: pillStyle }, engine === "cef" ? "CEF" : "WebKitGTK"),
          h("span", { style: pillStyle }, windowing === "wayland" ? "Wayland" : "X11"),
          h("span", { style: pillStyle }, "GPL-3.0")
        ),
        h(
          "div",
          { style: { display: "flex", gap: 10, flexWrap: "wrap", marginTop: 16 } },
          h("a", { href: repo, style: buttonStyle }, "Ver repositorio"),
          h(
            "a",
            { href: repo + "/issues", style: secondaryButtonStyle },
            "Reportar un problema"
          )
        ),
        h(
          "p",
          {
            style: {
              color: "var(--text-muted, #949ba4)",
              margin: "14px 0 0",
              fontWeight: 600,
            },
          },
          "Esta pagina es solo informativa: Vesk no acepta plugins ni temas."
        )
      ),
      art &&
        h("img", {
          src: art,
          alt: "Vesk",
          style: {
            width: 120,
            height: 120,
            borderRadius: 16,
            objectFit: "cover",
            flexShrink: 0,
          },
        })
    );

    const project = h(
      "section",
      { style: { ...cardStyle, marginTop: 16 } },
      h("h5", { style: titleStyle }, "Proyecto"),
      h(
        "p",
        { style: { margin: "0 0 8px", color: "var(--text-muted, #949ba4)" } },
        "Codigo fuente, issues y versiones."
      ),
      h(
        "p",
        { style: { margin: 0 } },
        h("a", { href: repo, style: linkStyle }, "github.com/staFF6773/vesk"),
        h("span", { style: { color: "var(--text-muted, #949ba4)" } }, " · "),
        h("a", { href: repo + "/issues", style: linkStyle }, "Issues")
      ),
      h(
        "p",
        { style: { margin: "8px 0 0", color: "var(--text-muted, #949ba4)" } },
        "Licencia GPL-3.0-or-later."
      )
    );

    return h(
      "div",
      {
        style: {
          padding: "24px 40px 40px",
          maxWidth: 780,
          fontSize: 14,
          lineHeight: 1.5,
          color: "var(--text-normal, #dbdee1)",
        },
      },
      banner,
      h(
        "div",
        {
          style: {
            display: "grid",
            gridTemplateColumns: "repeat(auto-fit, minmax(280px, 1fr))",
            gap: 16,
            marginTop: 16,
          },
        },
        group("Que funciona", engine === "cef" ? [
          "Cliente web de Discord y enlaces discord://",
          "Vencord inyectado al inicio del documento",
          "Rich Presence de juegos y apps (arRPC)",
          "Pausa videos y GIFs fuera de pantalla",
          "Reproduce GIFs y videos H.264/AAC",
          "Voz y video con WebRTC de Chromium (CEF)",
          "Compartir pantalla con el audio del sistema (PipeWire)",
          "Bandeja del sistema: mostrar, recargar, cerrar sesion, salir",
          windowing === "wayland"
            ? "Sesion persistente; el compositor decide tamano y posicion"
            : "Sesion persistente y tamano/posicion de ventana",
          "Enlaces externos en el navegador del sistema",
        ] : [
          "Cliente web de Discord y enlaces discord://",
          "Vencord inyectado al inicio del documento",
          "Rich Presence de juegos y apps (arRPC)",
          "Pausa videos y GIFs fuera de pantalla",
          "Bandeja del sistema: mostrar, recargar, cerrar sesion, salir",
          "Sesion persistente y tamano/posicion de ventana",
          "Enlaces externos en el navegador del sistema",
        ]),
        group("Limitaciones conocidas", engine === "cef" ? [
          "Al compartir pantalla se envia el audio de todo el sistema, no por aplicacion",
          "VESK_SCREEN_AUDIO=0 desactiva el audio al compartir pantalla",
          "Los GIFs pausados reinician su animacion al volver a pantalla",
          "Krisp no disponible; H.264 y AAC incluidos",
          "Notificaciones dependen del portal del escritorio",
          windowing === "wayland"
            ? "Wayland nativo: sin posicion global; el tamano se fija al crear la ventana"
            : "XWayland; VESK_GDK_BACKEND=wayland usa el backend nativo",
        ] : [
          "Compartir pantalla no funciona (WebKitGTK no implementa getDisplayMedia)",
          "Voz y video no disponibles: WebKitGTK no expone WebRTC",
          "Los GIFs pausados reinician su animacion al volver a pantalla",
          "Sin venmic ni codecs propietarios",
          "Notificaciones dependen del portal del escritorio",
          "Pagos, passkeys y captchas pueden detectar WebKit",
        ])
      ),
      project
    );
  };

  // Splash de carga: cubre el arranque de Discord con el arte del programa.
  (() => {
    const show = () => {
      if (document.getElementById("vesk-splash")) return;

      const style = document.createElement("style");
      style.textContent = `
        @keyframes vesk-spin { to { transform: rotate(360deg); } }
        @keyframes vesk-breathe {
          0%, 100% { transform: scale(1); opacity: .85; }
          50% { transform: scale(1.05); opacity: 1; }
        }
        #vesk-splash {
          position: fixed; inset: 0; z-index: 2147483647;
          display: flex; flex-direction: column; align-items: center; justify-content: center;
          gap: 18px; background: #1e1f22; color: #dbdee1;
          font-family: system-ui, sans-serif;
          transition: opacity .35s ease;
          /* Capa propia: el arranque de Discord repinta la pagina entera y sin
             esto el splash se redibuja con ella. */
          transform: translateZ(0);
        }
        #vesk-splash.vesk-hide { opacity: 0; pointer-events: none; }
        #vesk-splash .vesk-ring {
          position: relative; width: 136px; height: 136px;
          display: grid; place-items: center;
        }
        #vesk-splash .vesk-ring::before {
          content: ""; position: absolute; inset: 0; border-radius: 50%;
          border: 3px solid rgba(255, 255, 255, .12);
          border-top-color: #5865f2; border-right-color: #5865f2;
          /* Sin translateZ(0) WebKit no promociona la capa y repinta el anillo
             en el hilo principal: se congela mientras Discord arranca. */
          transform: translateZ(0);
          animation: vesk-spin 1s linear infinite;
        }
        #vesk-splash img {
          width: 104px; height: 104px; border-radius: 50%; object-fit: cover;
          transform: translateZ(0);
          animation: vesk-breathe 2.4s ease-in-out infinite;
        }
        #vesk-splash .vesk-name {
          font-size: 18px; font-weight: 600; letter-spacing: .02em;
        }
        #vesk-splash .vesk-status { font-size: 13px; color: #949ba4; margin-top: -10px; }
      `;

      const ring = document.createElement("div");
      ring.className = "vesk-ring";

      const img = document.createElement("img");
      img.alt = "";
      img.src = (window.__VESK__ && window.__VESK__.art) || "";
      ring.appendChild(img);

      const name = document.createElement("div");
      name.className = "vesk-name";
      name.textContent = "Vesk";

      const status = document.createElement("div");
      status.className = "vesk-status";
      status.textContent = "Cargando Discord...";

      const root = document.createElement("div");
      root.id = "vesk-splash";
      root.append(ring, name, status);
      document.body.append(style, root);

      const started = Date.now();
      let loaded = document.readyState === "complete";
      let done = false;

      const hide = () => {
        if (done) return;
        done = true;
        root.classList.add("vesk-hide");
        window.setTimeout(() => {
          root.remove();
          style.remove();
        }, 400);
      };

      window.addEventListener("load", () => {
        loaded = true;
      });

      const timer = window.setInterval(() => {
        if (done) {
          window.clearInterval(timer);
          return;
        }

        const elapsed = Date.now() - started;
        const mount = document.getElementById("app-mount");
        const rendered = !!mount && mount.childElementCount > 0;
        // Discord pinta su propio loader antes de la UI real: esperamos a la
        // pantalla de login (authBox) o a la barra de servidores (guilds).
        // Con el arbol vacio el selector no puede coincidir: nos ahorramos el
        // recorrido del DOM en cada tick mientras Discord monta.
        const ui =
          rendered &&
          !!mount.querySelector('[class*="authBox"], [class*="guilds"]');

        if (ui || (loaded && rendered && elapsed > 10000)) {
          window.clearInterval(timer);
          hide();
        }
      }, 200);

      // Red de seguridad: nunca dejar la app tapada por el splash.
      window.setTimeout(() => {
        window.clearInterval(timer);
        hide();
      }, 15000);
    };

    if (document.body) {
      show();
      return;
    }

    // initialization_script corre antes de que exista <html> (CEF crea el
    // documento vacio): observar documentElement no sirve si aun es null, y
    // observar document sin subtree solo ve aparecer <html>, no <body>. Con
    // subtree vemos <body> aparezca donde aparezca y desconectamos en el acto,
    // antes de que el parser llegue a los scripts de Discord.
    const observer = new MutationObserver(() => {
      if (!document.body) return;
      observer.disconnect();
      show();
    });
    observer.observe(document, { childList: true, subtree: true });
  })();
})();
"##;

    let art = vesk_art_data_uri();
    let engine = if cfg!(all(feature = "cef", target_os = "linux")) {
        "cef"
    } else {
        "webkit"
    };
    #[cfg(all(feature = "cef", target_os = "linux"))]
    let windowing = if cef::native_wayland() {
        "wayland"
    } else {
        "x11"
    };
    #[cfg(not(all(feature = "cef", target_os = "linux")))]
    let windowing = if std::env::var("GDK_BACKEND").as_deref() == Ok("wayland") {
        "wayland"
    } else {
        "x11"
    };
    let bridge = BRIDGE
        .replace("__VESK_ENGINE__", engine)
        .replace("__VESK_WINDOWING__", windowing)
        .replace("__VESK_VERSION__", env!("CARGO_PKG_VERSION"))
        .replace("__VESK_ICON__", art)
        .replace("__VESK_REPO__", VESK_REPO)
        .replace(
            "__VESK_ARRPC__",
            if arrpc::enabled() { "true" } else { "false" },
        )
        .replace(
            "__VESK_NO_TRACK__",
            if env_flag("VESK_NO_TRACK", true) {
                "true"
            } else {
                "false"
            },
        )
        .replace(
            "__VESK_DOM_OPTIMIZER__",
            if env_flag("VESK_DOM_OPTIMIZER", true) {
                "true"
            } else {
                "false"
            },
        )
        .replace(
            "__VESK_MEDIA_OPTIMIZER__",
            if env_flag("VESK_MEDIA_OPTIMIZER", true) {
                "true"
            } else {
                "false"
            },
        );
    let bundle = vencord_bundle();

    let mut script = String::with_capacity(prefix.len() + bridge.len() + bundle.len() + 1);
    script.push_str(&prefix);
    script.push_str(&bridge);
    script.push('\n');
    script.push_str(&bundle);
    script
}

pub(crate) fn is_discord_url(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };

    if !matches!(url.scheme(), "http" | "https") {
        return true;
    }

    let Some(host) = url.host_str() else {
        return false;
    };

    host == "discord.com"
        || host.ends_with(".discord.com")
        || host == "discordapp.com"
        || host.ends_with(".discordapp.com")
        || host == "discord.gg"
}

/// discord://-/channels/123/456 -> https://discord.com/channels/123/456
fn discord_deep_link_to_https(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix("discord://")?;
    let rest = rest.trim_start_matches('/');
    let rest = rest.strip_prefix("-/").unwrap_or(rest);
    let rest = rest.strip_prefix("discord.com/").unwrap_or(rest);
    let rest = rest.trim_start_matches('/');

    if rest.is_empty() {
        Some(DISCORD_URL.to_owned())
    } else {
        Some(format!("https://discord.com/{rest}"))
    }
}

fn navigate_deep_link(app: &AppHandle, raw: &str) {
    let Some(target) = discord_deep_link_to_https(raw) else {
        return;
    };

    if let Some(window) = app.get_webview_window("main") {
        let quoted = serde_json::to_string(&target)
            .unwrap_or_else(|_| "\"https://discord.com/app\"".to_owned());
        let _ = window.eval(format!("window.location.href = {quoted};"));
    }
}

fn hide_to_tray(window: &WebviewWindow) {
    // Avisa a CEF antes de desmapear: la pagina pasa a oculta y Chromium deja
    // de componer y de correr los timers de Discord a ritmo normal.
    let _ = AsRef::<tauri::Webview<BrowserEngine>>::as_ref(window).hide();
    let _ = window.hide();
}

fn show_from_tray(window: &WebviewWindow) {
    let _ = window.show();
    let _ = AsRef::<tauri::Webview<BrowserEngine>>::as_ref(window).show();
}

fn reveal(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        show_from_tray(&window);
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn install_close_to_tray(window: &WebviewWindow) {
    let win = window.clone();
    window.on_window_event(move |event| {
        let WindowEvent::CloseRequested { api, .. } = event else {
            return;
        };

        // Con VESK_CLOSE_TO_TRAY=0 la X cierra de verdad.
        if std::env::var("VESK_CLOSE_TO_TRAY").as_deref() == Ok("0") {
            return;
        }

        api.prevent_close();

        // Ocultar desde dentro del callback de cierre no surte efecto con este
        // runtime: la ventana se queda mapeada y el hilo principal empieza a
        // girar. Lo diferimos a un hilo, que despacha el mensaje ya fuera del
        // evento.
        let win = win.clone();
        std::thread::spawn(move || hide_to_tray(&win));

        if !TRAY_HINTED.swap(true, Ordering::Relaxed) {
            eprintln!("[vesk] ventana oculta en la bandeja del sistema");
        }
    });
}

fn create_tray(app: &App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Mostrar Vesk", true, None::<&str>)?;
    let reload = MenuItem::with_id(app, "reload", "Recargar", true, None::<&str>)?;
    let logout = MenuItem::with_id(app, "logout", "Cerrar sesion", true, None::<&str>)?;
    let devtools = MenuItem::with_id(
        app,
        "devtools",
        "Herramientas de desarrollo",
        true,
        None::<&str>,
    )?;
    let audio_system = MenuItem::with_id(app, "audio-system", "Sistema (menos Vesk)", true, None::<&str>)?;
    let audio_pick = MenuItem::with_id(app, "audio-pick", "Elegir aplicacion…", true, None::<&str>)?;
    let audio_off = MenuItem::with_id(app, "audio-off", "Desactivado", true, None::<&str>)?;
    let audio_menu = Submenu::with_id(
        app,
        "audio",
        "Audio al compartir",
        true,
    )?;
    audio_menu.append_items(&[&audio_system, &audio_pick, &audio_off])?;
    let arrpc_toggle = CheckMenuItem::with_id(
        app,
        "arrpc-toggle",
        "Actividad de juegos",
        true,
        arrpc::enabled(),
        None::<&str>,
    )?;
    let arrpc_menu = Submenu::with_id(app, "rich-presence", "Rich Presence", true)?;
    arrpc_menu.append_items(&[&arrpc_toggle])?;
    let arrpc_toggle_event = arrpc_toggle.clone();
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Salir", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show,
            &reload,
            &logout,
            &devtools,
            &arrpc_menu,
            &audio_menu,
            &separator,
            &quit,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id("vesk-tray")
        .tooltip("Vesk")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "show" => reveal(app),
            "reload" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.eval("window.location.reload()");
                }
            }
            "logout" => {
                if let Some(vault) = app.try_state::<SessionVault>() {
                    vault.forget();
                }
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.eval("window.location.href = 'https://discord.com/logout'");
                }
            }
            "devtools" => {
                if let Some(window) = app.get_webview_window("main") {
                    window.open_devtools();
                }
            }
            "quit" => {
                if let Some(vault) = app.try_state::<SessionVault>() {
                    vault.capture_now();
                }
                app.exit(0);
            }
            "arrpc-toggle" => {
                let active = arrpc::set_enabled(!arrpc::enabled());
                let _ = arrpc_toggle_event.set_checked(active);
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.eval(format!(
                        "if (window.__VESK_RICH_PRESENCE__) window.__VESK_RICH_PRESENCE__.setEnabled({active});"
                    ));
                }
            }
            "audio-system" => screen_audio::set_mode(Mode::System),
            "audio-off" => screen_audio::set_mode(Mode::Off),
            "audio-pick" => open_audio_picker(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                reveal(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }

    builder.build(app)?;
    Ok(())
}

/// Abre (o enfoca) la ventana emergente para elegir que aplicacion comparte audio.
fn open_audio_picker(app: &AppHandle) {
    if let Some(existing) = app.get_webview_window("audio-picker") {
        let _ = existing.show();
        let _ = existing.set_focus();
        return;
    }

    let html = picker_html();
    let path = app
        .path()
        .app_local_data_dir()
        .map(|dir| {
            let _ = std::fs::create_dir_all(&dir);
            dir.join("audio-picker.html")
        })
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/vesk-audio-picker.html"));

    if std::fs::write(&path, html).is_err() {
        return;
    }

    let url = format!("file://{}", path.display());
    let parsed = url::Url::parse(&url).unwrap_or_else(|_| url::Url::parse("file:///tmp/vesk-audio-picker.html").unwrap());
    let _ = WebviewWindowBuilder::new(app, "audio-picker", WebviewUrl::External(parsed))
        .title("Audio al compartir - Vesk")
        .inner_size(360.0, 440.0)
        .build();
}

/// HTML de la ventana de seleccion de audio. La lista de apps se incrusta en el
/// momento de abrir, asi que refleja el estado actual de PipeWire.
fn picker_html() -> String {
    let mut buttons = String::new();
    for app in screen_audio::list_apps() {
        let name = escape_html(&app.name);
        buttons.push_str(&format!(
            "<button class=\"app\" onclick=\"pick('app:{name}')\">{name}</button>\n",
            name = name
        ));
    }

    format!(
        r#"<!doctype html><html lang="es"><head><meta charset="utf-8">
<style>
  body {{ font-family: sans-serif; margin: 0; padding: 14px; color: #e6e6e6; background: #1e1f22; }}
  h3 {{ margin: 0 0 10px; font-size: 15px; }}
  button {{ display: block; width: 100%; text-align: left; margin: 4px 0; padding: 8px 10px;
    border: 0; border-radius: 6px; background: #2b2d31; color: #e6e6e6; cursor: pointer; font-size: 13px; }}
  button:hover {{ background: #3a3c42; }}
  .global {{ background: #5865f2; }}
  .global:hover {{ background: #4752c4; }}
  hr {{ border: 0; border-top: 1px solid #3a3c42; margin: 10px 0; }}
  p {{ color: #9aa0a6; font-size: 12px; margin: 6px 0; }}
</style></head><body>
  <h3>Audio al compartir pantalla</h3>
  <button class="global" onclick="pick('system')">Sistema (menos Vesk)</button>
  <button class="global" onclick="pick('off')">Desactivado</button>
  <hr>
  <p>Aplicaciones (excluye Vesk para evitar eco):</p>
  {buttons}
  <script>
    function pick(mode) {{
      window.__TAURI_INTERNALS__.invoke("screen_audio_set", {{ mode: mode }})
        .then(function () {{ window.close(); }})
        .catch(function (e) {{ console.error(e); }});
    }}
  </script>
</body></html>"#
    , buttons = buttons
    )
}

fn escape_html(value: &str) -> String {
    value
        .replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace("\"", "&quot;")
        .replace("'", "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traduce_deep_links() {
        assert_eq!(
            discord_deep_link_to_https("discord://-/channels/1/2").as_deref(),
            Some("https://discord.com/channels/1/2")
        );
        assert_eq!(
            discord_deep_link_to_https("discord://discord.com/channels/@me").as_deref(),
            Some("https://discord.com/channels/@me")
        );
        assert_eq!(
            discord_deep_link_to_https("discord:///channels/1").as_deref(),
            Some("https://discord.com/channels/1")
        );
        assert_eq!(
            discord_deep_link_to_https("discord://-/").as_deref(),
            Some(DISCORD_URL)
        );
        assert_eq!(discord_deep_link_to_https("https://discord.com"), None);
    }

    #[test]
    fn ajustes_solo_informativos() {
        let bundle = vencord_bundle();
        assert!(bundle.contains(r#"title:"Vesk",panelTitle:"Acerca de Vesk""#));
        assert!(bundle.contains("window.__VESK_ABOUT__"));
        assert!(!bundle.contains(r#"key:"vencord_main""#));
        assert!(!bundle.contains(r#"title:"Plugins",Component:wa"#));
        assert!(!bundle.contains(r#"title:"Themes",Component:hs"#));
        assert!(!bundle.contains(r#"key:"vencord_backup_restore""#));
    }

    #[test]
    fn codifica_base64() {
        assert_eq!(
            data_uri("text/plain", b"Man"),
            "data:text/plain;base64,TWFu"
        );
        assert_eq!(data_uri("text/plain", b"Ma"), "data:text/plain;base64,TWE=");
        assert_eq!(data_uri("text/plain", b"M"), "data:text/plain;base64,TQ==");
    }

    #[test]
    fn banner_usa_el_arte_del_programa() {
        let script = injection_script(None);
        assert!(!script.contains("__VESK_ICON__"));
        assert!(!script.contains("__VESK_VERSION__"));
        assert!(!script.contains("__VESK_WINDOWING__"));
        assert!(!script.contains("__VESK_ARRPC__"));
        assert!(script.contains(r#"data:image/jpeg;base64,"#));
        assert!(script.contains("window.__VESK_ABOUT__"));
    }

    #[test]
    fn splash_de_carga() {
        let script = injection_script(None);
        assert!(script.contains("vesk-splash"));
        assert!(script.contains("@keyframes vesk-spin"));
        assert!(script.contains("Cargando Discord..."));
        // CEF ejecuta el script antes de que exista <html>: el observador tiene
        // que mirar el subarbol de document o <body> nunca se ve.
        assert!(script.contains("subtree: true"));
    }

    #[test]
    fn arte_ligero() {
        let uri = vesk_art_data_uri();
        assert!(uri.starts_with("data:image/jpeg;base64,"));
        // 256x256: ~30 KB en base64. Si vuelve a crecer, el script de
        // inyeccion se hincha en cada documento.
        assert!(uri.len() < 40_000, "el arte embebido pesa {}", uri.len());
    }

    #[test]
    fn parches_de_arranque() {
        let script = injection_script(None);
        assert!(script.contains("__SENTRY__"));
        assert!(script.contains("science|metrics"));
        assert!(script.contains("removeChild"));
        assert!(script.contains("preconnect"));
        assert!(!script.contains("__VESK_NO_TRACK__"));
        assert!(!script.contains("__VESK_DOM_OPTIMIZER__"));
        assert!(!script.contains("__VESK_MEDIA_OPTIMIZER__"));
        assert!(!script.contains("__VESK_REPO__"));
        assert!(!script.contains("__VESK_ARRPC__"));
        assert!(script.contains("window.__VESK_RICH_PRESENCE__"));
        assert!(script.contains("setEnabled"));
        assert!(script.contains("activity: null"));
        assert!(script.contains("WebRichPresence (arRPC)"));
        assert!(script.contains("ws://127.0.0.1:1337"));
    }

    #[test]
    fn vencord_incluye_arrpc() {
        assert!(VENCORD_BUNDLE.contains("WebRichPresence (arRPC)"));
        assert!(VENCORD_BUNDLE.contains("ws://127.0.0.1:1337"));
        assert!(VENCORD_BUNDLE.contains("handleEvent"));
    }

    #[test]
    fn acerca_enlaza_al_repositorio() {
        let script = injection_script(None);
        assert!(script.contains(VESK_REPO));
        assert!(script.contains("Ver repositorio"));
        assert!(script.contains("repo:"));
    }

    #[test]
    fn optimizador_de_medios() {
        let script = injection_script(None);
        assert!(script.contains("IntersectionObserver"));
        assert!(script.contains("visibilitychange"));
        assert!(script.contains("srcObject"));
        assert!(script.contains("data:image/gif;base64,"));
        assert!(!script.contains("__VESK_MEDIA_OPTIMIZER__"));
    }

    #[test]
    fn filtra_navegacion() {
        assert!(is_discord_url("https://discord.com/app"));
        assert!(is_discord_url("https://canary.discord.com/channels/@me"));
        assert!(is_discord_url(
            "https://cdn.discordapp.com/attachments/1/2.png"
        ));
        assert!(!is_discord_url("https://example.com"));
        assert!(!is_discord_url("no es una url"));
    }
}

