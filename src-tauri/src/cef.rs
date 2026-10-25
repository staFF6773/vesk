use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use tauri_runtime_cef::{
    CefConfig, DenyReason, LinuxWindowing, NormalizedOrigin, PermissionKind,
    DEFAULT_PROMPT_TIMEOUT,
};

const DISCORD_HOSTS: &[&str] = &["discord.com", "discordapp.com", "discord.gg"];

/// Sistema de ventanas con el que CEF crea la ventana de nivel superior.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Windowing {
    X11,
    Wayland,
}

/// VESK_GDK_BACKEND fuerza un backend concreto ("wayland" o "x11"). Sin valor
/// se usa Wayland nativo si la sesion lo ofrece y X11 en caso contrario: en
/// Wayland, CEF crea su propia ventana (CEF Views) y no pasa por XWayland.
fn windowing() -> Windowing {
    match std::env::var("VESK_GDK_BACKEND") {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "wayland" => Windowing::Wayland,
            "x11" | "xwayland" => Windowing::X11,
            _ => detect_windowing(),
        },
        Err(_) => detect_windowing(),
    }
}

fn detect_windowing() -> Windowing {
    let wayland_session = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE")
            .map(|value| value.eq_ignore_ascii_case("wayland"))
            .unwrap_or(false);

    if wayland_session {
        Windowing::Wayland
    } else {
        Windowing::X11
    }
}

pub fn native_wayland() -> bool {
    windowing() == Windowing::Wayland
}

pub fn configure_env() {
    // GTK (bandeja del sistema) tiene que hablar el mismo protocolo que CEF.
    std::env::set_var(
        "GDK_BACKEND",
        if native_wayland() { "wayland" } else { "x11" },
    );

    if std::env::var_os("__NV_DISABLE_EXPLICIT_SYNC").is_none() {
        std::env::set_var("__NV_DISABLE_EXPLICIT_SYNC", "1");
    }

    // 30 ms de buffer de audio: lo mismo que fija OpenAsar para que la voz no
    // acumule latencia en PulseAudio/PipeWire.
    if std::env::var_os("PULSE_LATENCY_MSEC").is_none() {
        std::env::set_var("PULSE_LATENCY_MSEC", "30");
    }
}

pub fn configure() {
    tauri_runtime_cef::configure(CefConfig {
        identifier: "dev.vencord.vesk".into(),
        custom_schemes: vec!["tauri".into(), "ipc".into(), "asset".into()],
        command_line_args: command_line_args(),
        linux_windowing: if native_wayland() {
            LinuxWindowing::Wayland
        } else {
            LinuxWindowing::X11
        },
        ..Default::default()
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GpuPreset {
    /// Sin flags nuevos: el comportamiento de siempre.
    Off,
    /// Rasterizado por GPU y mejoras que no tocan GpuMemoryBuffer.
    Safe,
    /// Preset completo de OpenAsar: zero-copy, overlays y sin blocklist.
    Full,
}

/// El preset completo pide GpuMemoryBuffer para zero-copy y overlays. El driver
/// propietario de NVIDIA bajo X11 no lo expone (gmb_type vacio), asi que el
/// proceso GPU muere con SIGTRAP en cuanto se rasteriza un canvas. En ese caso
/// arrancamos en conservador y el preset completo queda como opt-in.
fn gpu_preset() -> GpuPreset {
    match std::env::var("VESK_GPU_PERF") {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "no" | "off" => GpuPreset::Off,
            "full" | "aggressive" | "openasar" => GpuPreset::Full,
            _ => GpuPreset::Safe,
        },
        Err(_) => {
            if std::path::Path::new("/dev/nvidiactl").exists() {
                GpuPreset::Safe
            } else {
                GpuPreset::Full
            }
        }
    }
}

fn command_line_args() -> Vec<(String, Option<String>)> {
    // El runtime fusiona disable-features/enable-features con las que ya fija
    // CEF, asi que basta con una entrada por switch.
    // Sin IntensiveWakeUpThrottling en la lista: cuando la ventana se oculta
    // en la bandeja, Chromium debe poder estrangular los timers de Discord.
    // El runtime ya avisa a CEF con was_hidden(), asi que el aviso llega; lo
    // unico que lo anulaba era desactivar el throttling.
    let mut disabled = String::from(
        "SpareRendererForSitePerProcess,\
         AllowAggressiveThrottlingWithWebSocket,UseEcoQoSForBackgroundProcess,\
         LocalNetworkAccessChecks,LocalNetworkAccessChecksWebSocket,\
         LocalNetworkAccessChecksWebRTC",
    )
    .replace(' ', "");

    let mut enabled = String::new();
    let mut args = vec![
        ("--disable-gpu-sandbox".into(), None),
        ("--no-first-run".into(), None),
        ("--no-default-browser-check".into(), None),
        (
            "autoplay-policy".into(),
            Some("no-user-gesture-required".into()),
        ),
        ("--renderer-process-limit".into(), Some("2".into())),
    ];

    match gpu_preset() {
        GpuPreset::Off => {}
        GpuPreset::Safe => {
            args.push(("--enable-gpu-rasterization".into(), None));
            enabled.push_str(
                "ThrottleDisplayNoneAndVisibilityHiddenCrossOriginIframes,\
                 WebAssemblyLazyCompilation",
            );
        }
        GpuPreset::Full => {
            args.extend([
                ("--enable-gpu-rasterization".into(), None),
                ("--enable-zero-copy".into(), None),
                ("--ignore-gpu-blocklist".into(), None),
                (
                    "--enable-hardware-overlays".into(),
                    Some("single-fullscreen,single-on-top,underlay".into()),
                ),
            ]);
            enabled.push_str(
                "EnableDrDc,CanvasOopRasterization,\
                 ThrottleDisplayNoneAndVisibilityHiddenCrossOriginIframes,\
                 WebAssemblyLazyCompilation",
            );
            disabled.push_str(",Vulkan");
        }
    }

    if !enabled.is_empty() {
        args.push(("enable-features".into(), Some(enabled.replace(' ', ""))));
    }

    args.push(("disable-features".into(), Some(disabled)));

    if let Ok(extra) = std::env::var("VESK_CEF_ARGS") {
        for arg in extra.split(',').map(str::trim).filter(|arg| !arg.is_empty()) {
            match arg.split_once('=') {
                Some((key, value)) => {
                    args.push((normalize_switch(key), Some(value.to_owned())));
                }
                None => args.push((normalize_switch(arg), None)),
            }
        }
    }

    args
}

fn normalize_switch(raw: &str) -> String {
    if raw.starts_with('-') {
        raw.to_owned()
    } else {
        format!("--{raw}")
    }
}

pub fn install_permission_policy() {
    static GRANTED: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let granted = GRANTED.get_or_init(|| Mutex::new(HashSet::new()));

    tauri_runtime_cef::set_permission_policy(move |request, responder| {
        if request.webview_label != "main" {
            return responder.deny(DenyReason::NoPolicy);
        }

        let Some(origin) = request.origin.as_ref() else {
            return responder.deny(DenyReason::InvalidOrigin);
        };

        if !is_discord_origin(origin) {
            return responder.deny(DenyReason::NoPolicy);
        }

        let kinds: Vec<PermissionKind> = request
            .kinds
            .iter()
            .filter(|kind| is_media_kind(kind))
            .cloned()
            .collect();

        if kinds.is_empty() || kinds.len() != request.kinds.len() {
            return responder.deny(DenyReason::UnsupportedPermission);
        }

        let names: Vec<&'static str> = kinds.iter().map(kind_name).collect();

        {
            let cache = granted.lock().expect("cef permission cache");
            if names.iter().all(|name| cache.contains(name)) {
                return responder.allow();
            }
        }

        let message = prompt_message(&kinds);
        let deferred = responder.defer(DEFAULT_PROMPT_TIMEOUT);
        let (tx, rx) = std::sync::mpsc::channel();
        let names_to_cache = names.clone();

        prompt(message, tx);

        std::thread::spawn(move || {
            let allowed = rx
                .recv_timeout(DEFAULT_PROMPT_TIMEOUT)
                .unwrap_or(false);

            if allowed {
                let mut cache = granted.lock().expect("cef permission cache");
                for name in names_to_cache {
                    cache.insert(name);
                }
                deferred.allow();
            } else {
                deferred.deny(DenyReason::UserDenied);
            }
        });
    });

    tauri_runtime_cef::set_permission_audit(|event| {
        eprintln!(
            "[vesk] permiso {} en {}: concedido={} tipos={:?}",
            event.request_id,
            event.raw_origin,
            event.granted,
            event.kinds
        );
    });
}

pub fn install_popup_policy() {
    tauri_runtime_cef::set_popup_policy(|request| {
        if !crate::is_discord_url(request.url) {
            if let Ok(url) = url::Url::parse(request.url) {
                if matches!(url.scheme(), "http" | "https") {
                    let target = url.to_string();
                    std::thread::spawn(move || {
                        let _ = open::that(target);
                    });
                }
            }
        }

        false
    });
}

fn is_discord_origin(origin: &NormalizedOrigin) -> bool {
    if !matches!(origin.scheme.as_str(), "http" | "https") {
        return false;
    }

    let host = origin.host.trim_end_matches('.').to_ascii_lowercase();
    DISCORD_HOSTS
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

fn is_media_kind(kind: &PermissionKind) -> bool {
    matches!(
        kind,
        PermissionKind::Microphone
            | PermissionKind::Camera
            | PermissionKind::CameraPanTiltZoom
            | PermissionKind::ScreenCapture
            | PermissionKind::CapturedSurfaceControl
    )
}

fn kind_name(kind: &PermissionKind) -> &'static str {
    match kind {
        PermissionKind::Microphone => "microphone",
        PermissionKind::Camera | PermissionKind::CameraPanTiltZoom => "camera",
        PermissionKind::ScreenCapture | PermissionKind::CapturedSurfaceControl => "screen",
        _ => "other",
    }
}

fn prompt_message(kinds: &[PermissionKind]) -> String {
    match kinds {
        [PermissionKind::Microphone] => "Vesk quiere usar el microfono.".to_owned(),
        [PermissionKind::Camera] => "Vesk quiere usar la camara.".to_owned(),
        [PermissionKind::ScreenCapture] | [PermissionKind::CapturedSurfaceControl] => {
            "Vesk quiere compartir tu pantalla.".to_owned()
        }
        _ if kinds.contains(&PermissionKind::Microphone)
            && kinds.contains(&PermissionKind::Camera) =>
        {
            "Vesk quiere usar el microfono y la camara.".to_owned()
        }
        _ => "Vesk quiere acceder a tus dispositivos de audio o video.".to_owned(),
    }
}

fn prompt(message: String, tx: std::sync::mpsc::Sender<bool>) {
    use gtk::glib;
    use gtk::prelude::*;
    use gtk::{
        ButtonsType, DialogFlags, MessageDialog, MessageType, ResponseType, WindowPosition,
    };

    glib::idle_add_once(move || {
        let dialog = MessageDialog::new(
            None::<&gtk::Window>,
            DialogFlags::MODAL,
            MessageType::Question,
            ButtonsType::YesNo,
            &message,
        );
        dialog.set_title("Permiso de Vesk");
        dialog.set_position(WindowPosition::CenterAlways);

        let tx = std::cell::RefCell::new(Some(tx));
        dialog.connect_response(move |dialog, response| {
            if let Some(tx) = tx.take() {
                let _ = tx.send(matches!(response, ResponseType::Yes));
            }
            dialog.close();
        });

        dialog.show();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(raw: &str) -> NormalizedOrigin {
        NormalizedOrigin::parse(raw).expect("origen valido")
    }

    #[test]
    fn acepta_origenes_de_discord() {
        assert!(is_discord_origin(&origin("https://discord.com")));
        assert!(is_discord_origin(&origin("https://canary.discord.com")));
        assert!(is_discord_origin(&origin("https://ptb.discord.com")));
        assert!(is_discord_origin(&origin("https://discordapp.com")));
        assert!(is_discord_origin(&origin("https://cdn.discordapp.com")));
    }

    fn arg<'a>(args: &'a [(String, Option<String>)], name: &str) -> &'a str {
        args.iter()
            .find(|(key, _)| key == name)
            .and_then(|(_, value)| value.as_deref())
            .unwrap_or("")
    }

    fn args_for(preset: &str) -> Vec<(String, Option<String>)> {
        std::env::set_var("VESK_GPU_PERF", preset);
        std::env::remove_var("VESK_CEF_ARGS");
        command_line_args()
    }

    fn has(args: &[(String, Option<String>)], name: &str) -> bool {
        args.iter().any(|(key, _)| key == name)
    }

    #[test]
    fn presets_de_gpu() {
        // Completo: todo el preset de OpenAsar.
        let full = args_for("full");
        for flag in [
            "--enable-gpu-rasterization",
            "--enable-zero-copy",
            "--ignore-gpu-blocklist",
        ] {
            assert!(has(&full, flag), "falta {flag}");
        }
        assert!(arg(&full, "enable-features").contains("EnableDrDc"));
        assert!(arg(&full, "disable-features").contains("Vulkan"));

        // Conservador: nada que pida GpuMemoryBuffer (el crash con NVIDIA).
        let safe = args_for("safe");
        assert!(has(&safe, "--enable-gpu-rasterization"));
        assert!(!has(&safe, "--enable-zero-copy"));
        assert!(!has(&safe, "--ignore-gpu-blocklist"));
        assert!(!arg(&safe, "enable-features").contains("EnableDrDc"));
        assert!(!arg(&safe, "disable-features").contains("Vulkan"));

        // Apagado: solo los flags de siempre.
        let off = args_for("0");
        assert!(!has(&off, "--enable-gpu-rasterization"));
        assert!(arg(&off, "enable-features").is_empty());

        std::env::remove_var("VESK_GPU_PERF");
    }

    #[test]
    fn un_solo_disable_features() {
        std::env::remove_var("VESK_CEF_ARGS");
        let args = command_line_args();
        let count = args
            .iter()
            .filter(|(key, _)| key == "disable-features")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn rechaza_origenes_ajenos() {
        assert!(!is_discord_origin(&origin("https://example.com")));
        assert!(!is_discord_origin(&origin("https://discord.com.evil.test")));
        assert!(!is_discord_origin(&origin("https://evil-discord.com")));
        assert!(!is_discord_origin(&origin("http://localhost:1420")));
        assert!(!is_discord_origin(&origin("wss://discord.com")));
    }
}
