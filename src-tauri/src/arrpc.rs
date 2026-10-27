//! Servidor local de Rich Presence compatible con arRPC.
//!
//! Los juegos y apps hablan por el socket IPC de Discord y el plugin
//! `WebRichPresence (arRPC)` de Vencord recibe la actividad por el bridge
//! WebSocket de `127.0.0.1:1337`. La tabla de deteccion es una copia recortada
//! de OpenAsar/arRPC (MIT), limitada a las entradas Linux.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as WsRequest, Response as WsResponse,
};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::env_flag;

const BRIDGE_ADDR: &str = "127.0.0.1:1337";
const DISCORD_ORIGINS: [&str; 3] = [
    "https://discord.com",
    "https://ptb.discord.com",
    "https://canary.discord.com",
];
const MAX_FRAME: usize = 16 * 1024 * 1024;
const IPC_HANDSHAKE: i32 = 0;
const IPC_FRAME: i32 = 1;
const IPC_CLOSE: i32 = 2;
const IPC_PING: i32 = 3;
const IPC_PONG: i32 = 4;

type Activities = Arc<Mutex<HashMap<String, String>>>;

static ENGINE: OnceLock<Mutex<Option<Engine>>> = OnceLock::new();
static SOCKET_ID: AtomicU64 = AtomicU64::new(1);
static PING_ID: AtomicU64 = AtomicU64::new(1);

struct Engine {
    stop: watch::Sender<bool>,
    handle: std::thread::JoinHandle<()>,
}

/// Arranca Rich Presence si `VESK_ARRPC` no lo desactiva.
pub fn init() {
    if env_flag("VESK_ARRPC", true) {
        set_enabled(true);
    }
}

/// Para el servidor y libera el socket IPC propio.
pub fn shutdown() {
    set_enabled(false);
}

/// Indica si el motor esta arrancado (aunque el bridge sea de otro arRPC).
pub fn enabled() -> bool {
    engine().lock().map(|slot| slot.is_some()).unwrap_or(false)
}

/// Arranca o para el motor. Devuelve si quedo activo.
pub fn set_enabled(on: bool) -> bool {
    let mut slot = match engine().lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };

    if on {
        if slot.is_none() {
            match Engine::start() {
                Ok(engine) => *slot = Some(engine),
                Err(error) => {
                    eprintln!("[vesk] arRPC no pudo arrancar: {error}");
                    return false;
                }
            }
        }
        true
    } else {
        if let Some(engine) = slot.take() {
            engine.stop();
        }
        false
    }
}

fn engine() -> &'static Mutex<Option<Engine>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

impl Engine {
    fn start() -> Result<Self, String> {
        let (stop_tx, stop_rx) = watch::channel(false);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let handle = std::thread::Builder::new()
            .name("vesk-arrpc".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!(
                            "no se pudo crear el runtime de Tokio: {error}"
                        )));
                        return;
                    }
                };

                runtime.block_on(serve(stop_rx, ready_tx));
            })
            .map_err(|error| format!("no se pudo crear el hilo: {error}"))?;

        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => Ok(Self {
                stop: stop_tx,
                handle,
            }),
            Ok(Err(error)) => {
                let _ = stop_tx.send(true);
                let _ = handle.join();
                Err(error)
            }
            Err(error) => {
                let _ = stop_tx.send(true);
                let _ = handle.join();
                Err(format!("el servidor no respondio al arrancar: {error}"))
            }
        }
    }

    fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.handle.join();
    }
}

async fn serve(
    mut stop: watch::Receiver<bool>,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    let ws_listener = match TcpListener::bind(BRIDGE_ADDR).await {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            // Si ya hay otro arRPC, el cliente inyectado se conecta a el y no
            // levantamos un segundo IPC que competiria por los sockets.
            if TcpStream::connect(BRIDGE_ADDR).await.is_ok() {
                eprintln!(
                    "[vesk] arRPC: {BRIDGE_ADDR} ya esta en uso; se usara la instancia externa"
                );
                let _ = ready.send(Ok(()));
                let _ = stop.changed().await;
                return;
            }

            let message = format!("{BRIDGE_ADDR} esta ocupado y no responde como WebSocket");
            let _ = ready.send(Err(message));
            return;
        }
        Err(error) => {
            let _ = ready.send(Err(format!("no se pudo abrir {BRIDGE_ADDR}: {error}")));
            return;
        }
    };

    let ipc = bind_ipc().await;
    let (activity_tx, _) = broadcast::channel::<String>(64);
    let activities: Activities = Arc::new(Mutex::new(HashMap::new()));

    let ws_task = tokio::spawn(accept_ws(
        ws_listener,
        activities.clone(),
        activity_tx.clone(),
    ));

    let mut ipc_path = None;
    let mut ipc_task = None;
    if let Some((path, listener)) = ipc {
        ipc_path = Some(path);
        ipc_task = Some(tokio::spawn(accept_ipc(
            listener,
            activities.clone(),
            activity_tx.clone(),
        )));
    } else {
        eprintln!("[vesk] arRPC: no hay socket IPC libre; solo funcionara el bridge");
    }

    let scanner = if env_flag("VESK_ARRPC_SCAN", true) {
        Some(tokio::spawn(scan_processes(
            activities.clone(),
            activity_tx.clone(),
        )))
    } else {
        None
    };

    eprintln!("[vesk] arRPC: escuchando en ws://{BRIDGE_ADDR}");
    let _ = ready.send(Ok(()));

    let _ = stop.changed().await;
    ws_task.abort();
    if let Some(task) = ipc_task {
        task.abort();
    }
    if let Some(task) = scanner {
        task.abort();
    }
    if let Some(path) = ipc_path {
        let _ = std::fs::remove_file(path);
    }
}

async fn accept_ws(
    listener: TcpListener,
    activities: Activities,
    activity_tx: broadcast::Sender<String>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle_ws(
                    stream,
                    activities.clone(),
                    activity_tx.subscribe(),
                ));
            }
            Err(error) => {
                eprintln!("[vesk] arRPC: error aceptando WebSocket: {error}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

#[allow(clippy::result_large_err)] // ErrorResponse de tungstenite
async fn handle_ws(
    stream: TcpStream,
    activities: Activities,
    mut activity_rx: broadcast::Receiver<String>,
) {
    let mut socket = match tokio_tungstenite::accept_hdr_async(
        stream,
        |request: &WsRequest, response: WsResponse| {
            let origin = request
                .headers()
                .get("origin")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");

            if origin.is_empty() || DISCORD_ORIGINS.contains(&origin) {
                return Ok(response);
            }

            eprintln!("[vesk] arRPC: origen WebSocket rechazado: {origin}");
            let mut error = ErrorResponse::new(Some("origen no permitido".to_owned()));
            *error.status_mut() = StatusCode::FORBIDDEN;
            Err(error)
        },
    )
    .await
    {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("[vesk] arRPC: handshake WebSocket invalido: {error}");
            return;
        }
    };

    let snapshot = activities
        .lock()
        .map(|activities| activities.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();

    for payload in snapshot {
        if socket.send(WsMessage::Text(payload.into())).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            message = activity_rx.recv() => match message {
                Ok(payload) => {
                    if socket.send(WsMessage::Text(payload.into())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
            incoming = socket.next() => match incoming {
                Some(Ok(WsMessage::Ping(payload))) => {
                    if socket.send(WsMessage::Pong(payload)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(WsMessage::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    eprintln!("[vesk] arRPC: WebSocket cerrado: {error}");
                    break;
                }
            }
        }
    }
}

async fn accept_ipc(
    listener: UnixListener,
    activities: Activities,
    activity_tx: broadcast::Sender<String>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle_ipc(stream, activities.clone(), activity_tx.clone()));
            }
            Err(error) => {
                eprintln!("[vesk] arRPC: error aceptando IPC: {error}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

async fn handle_ipc(
    mut stream: UnixStream,
    activities: Activities,
    activity_tx: broadcast::Sender<String>,
) {
    let socket_id = SOCKET_ID.fetch_add(1, Ordering::Relaxed).to_string();
    let mut client_id = String::new();
    let mut last_pid = None;
    let mut handshook = false;

    loop {
        let frame = match read_frame(&mut stream).await {
            Ok(frame) => frame,
            Err(error) => {
                eprintln!("[vesk] arRPC: error leyendo IPC: {error}");
                break;
            }
        };

        let Some((kind, data)) = frame else {
            break;
        };

        match kind {
            IPC_HANDSHAKE => {
                if handshook {
                    break;
                }
                let version = data.get("v").and_then(Value::as_u64).unwrap_or(1);
                if version != 1 {
                    eprintln!("[vesk] arRPC: version IPC no soportada: {version}");
                    break;
                }
                client_id = data
                    .get("client_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                handshook = true;
                if write_frame(&mut stream, IPC_FRAME, &ready_payload())
                    .await
                    .is_err()
                {
                    break;
                }
            }
            IPC_FRAME if handshook => {
                if handle_ipc_message(
                    &mut stream,
                    &socket_id,
                    &client_id,
                    &activities,
                    &activity_tx,
                    &data,
                    &mut last_pid,
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            IPC_PING => {
                let _ = write_frame(&mut stream, IPC_PONG, &data).await;
            }
            IPC_PONG => {}
            IPC_CLOSE => break,
            _ => {}
        }
    }

    if handshook {
        publish(&activities, &activity_tx, &socket_id, None, last_pid);
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_ipc_message(
    stream: &mut UnixStream,
    socket_id: &str,
    client_id: &str,
    activities: &Activities,
    activity_tx: &broadcast::Sender<String>,
    message: &Value,
    last_pid: &mut Option<u32>,
) -> io::Result<()> {
    let command = message.get("cmd").and_then(Value::as_str).unwrap_or("");
    let args = message.get("args").cloned().unwrap_or(Value::Null);
    let nonce = message.get("nonce").cloned().unwrap_or(Value::Null);
    let pid = args
        .get("pid")
        .and_then(Value::as_u64)
        .map(|pid| pid as u32);

    if let Some(pid) = pid {
        *last_pid = Some(pid);
    }

    match command {
        "SET_ACTIVITY" => {
            let activity = args.get("activity").cloned().unwrap_or(Value::Null);
            if activity.is_null() {
                publish(activities, activity_tx, socket_id, None, pid.or(*last_pid));
                let response = json!({
                    "cmd": command,
                    "data": Value::Null,
                    "evt": Value::Null,
                    "nonce": nonce,
                });
                write_frame(stream, IPC_FRAME, &response).await?;
            } else {
                let instance = activity
                    .get("instance")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let normalized = normalize_activity(client_id, activity, instance);
                publish(
                    activities,
                    activity_tx,
                    socket_id,
                    Some(normalized.clone()),
                    pid.or(*last_pid),
                );

                let mut echo = normalized;
                if let Some(object) = echo.as_object_mut() {
                    object.insert("name".to_owned(), json!(""));
                }
                let response = json!({
                    "cmd": command,
                    "data": echo,
                    "evt": Value::Null,
                    "nonce": nonce,
                });
                write_frame(stream, IPC_FRAME, &response).await?;
            }
        }
        "CONNECTIONS_CALLBACK" => {
            let response = json!({
                "cmd": command,
                "data": { "code": 1000 },
                "evt": "ERROR",
                "nonce": nonce,
            });
            write_frame(stream, IPC_FRAME, &response).await?;
        }
        "INVITE_BROWSER" | "GUILD_TEMPLATE_BROWSER" | "DEEP_LINK" => {
            let response = json!({
                "cmd": command,
                "data": {
                    "code": 4011,
                    "message": "Vesk solo implementa actividad Rich Presence",
                },
                "evt": "ERROR",
                "nonce": nonce,
            });
            write_frame(stream, IPC_FRAME, &response).await?;
        }
        other if !other.is_empty() => {
            eprintln!("[vesk] arRPC: comando IPC no soportado: {other}");
        }
        _ => {}
    }

    Ok(())
}

fn normalize_activity(client_id: &str, mut activity: Value, instance: bool) -> Value {
    if !activity.is_object() {
        return activity;
    }

    let buttons = activity
        .get("buttons")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let labels = buttons
        .iter()
        .filter_map(|button| button.get("label").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let urls = buttons
        .iter()
        .filter_map(|button| button.get("url").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<Vec<_>>();

    normalize_timestamps(&mut activity);

    let application_id = if client_id.is_empty() {
        activity
            .get("application_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    } else {
        client_id.to_owned()
    };

    let Some(object) = activity.as_object_mut() else {
        return activity;
    };

    object.insert("application_id".to_owned(), json!(application_id));
    object.insert("type".to_owned(), json!(0));
    object.insert("flags".to_owned(), json!(if instance { 1 } else { 0 }));
    object
        .entry("metadata".to_owned())
        .or_insert_with(|| json!({}));
    if !buttons.is_empty() {
        object.insert("metadata".to_owned(), json!({ "button_urls": urls }));
        object.insert("buttons".to_owned(), json!(labels));
    }

    activity
}

fn normalize_timestamps(activity: &mut Value) {
    let now_len = now_ms().to_string().len();
    let Some(timestamps) = activity
        .get_mut("timestamps")
        .and_then(Value::as_object_mut)
    else {
        return;
    };

    for value in timestamps.values_mut() {
        let Some(timestamp) = value.as_u64() else {
            continue;
        };
        if now_len.saturating_sub(timestamp.to_string().len()) > 2 {
            *value = json!(timestamp.saturating_mul(1000));
        }
    }
}

fn publish(
    activities: &Activities,
    activity_tx: &broadcast::Sender<String>,
    socket_id: &str,
    activity: Option<Value>,
    pid: Option<u32>,
) {
    let payload = json!({
        "activity": activity,
        "pid": pid,
        "socketId": socket_id,
    })
    .to_string();

    if let Ok(mut activities) = activities.lock() {
        if activity_is_present(&payload) {
            activities.insert(socket_id.to_owned(), payload.clone());
        } else {
            activities.remove(socket_id);
        }
    }

    let _ = activity_tx.send(payload);
}

fn activity_is_present(payload: &str) -> bool {
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|value| value.get("activity").map(|activity| !activity.is_null()))
        .unwrap_or(false)
}

fn ready_payload() -> Value {
    json!({
        "cmd": "DISPATCH",
        "data": {
            "v": 1,
            "config": {
                "cdn_host": "cdn.discordapp.com",
                "api_endpoint": "//discord.com/api",
                "environment": "production"
            },
            "user": {
                "id": "1045800378228281345",
                "username": "arrpc",
                "discriminator": "0",
                "global_name": "arRPC",
                "avatar": "cfefa4d9839fb4bdf030f91c2a13e95c",
                "avatar_decoration_data": null,
                "bot": false,
                "flags": 0,
                "premium_type": 0
            }
        },
        "evt": "READY",
        "nonce": null
    })
}

async fn read_frame<R>(reader: &mut R) -> io::Result<Option<(i32, Value)>>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; 8];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }

    let kind = i32::from_le_bytes(header[0..4].try_into().unwrap());
    let length = i32::from_le_bytes(header[4..8].try_into().unwrap());
    if length < 0 || length as usize > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "tamano de frame IPC invalido",
        ));
    }

    let mut body = vec![0u8; length as usize];
    reader.read_exact(&mut body).await?;
    let value = serde_json::from_slice(&body)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

    Ok(Some((kind, value)))
}

async fn write_frame<W>(writer: &mut W, kind: i32, value: &Value) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let length = i32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame IPC demasiado grande"))?;

    let mut header = [0u8; 8];
    header[0..4].copy_from_slice(&kind.to_le_bytes());
    header[4..8].copy_from_slice(&length.to_le_bytes());

    writer.write_all(&header).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

async fn bind_ipc() -> Option<(PathBuf, UnixListener)> {
    let runtime_dir = runtime_dir();

    for index in 0..10 {
        let path = runtime_dir.join(format!("discord-ipc-{index}"));

        if path.exists() {
            match UnixStream::connect(&path).await {
                Ok(mut stream) => {
                    let ping = json!(PING_ID.fetch_add(1, Ordering::Relaxed));
                    let alive = if write_frame(&mut stream, IPC_PING, &ping).await.is_ok() {
                        matches!(
                            timeout(Duration::from_millis(500), read_frame(&mut stream)).await,
                            Ok(Ok(Some((IPC_PONG, _))))
                        )
                    } else {
                        false
                    };

                    if alive {
                        continue;
                    }

                    // El socket existe y acepta conexiones: puede ser otro
                    // servidor IPC lento. No lo borramos; probamos el siguiente.
                    eprintln!(
                        "[vesk] arRPC: {} no responde al ping; se omite",
                        path.display()
                    );
                    continue;
                }
                Err(_) => {
                    // Sin listener: el archivo quedo de una sesion anterior.
                    let _ = std::fs::remove_file(&path);
                }
            }
        }

        match UnixListener::bind(&path) {
            Ok(listener) => {
                set_socket_permissions(&path);
                eprintln!("[vesk] arRPC: IPC en {}", path.display());
                return Some((path, listener));
            }
            Err(error) => {
                eprintln!("[vesk] arRPC: no se pudo abrir {}: {error}", path.display());
            }
        }
    }

    None
}

fn runtime_dir() -> PathBuf {
    for name in ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"] {
        if let Some(value) = std::env::var_os(name) {
            if !value.is_empty() {
                return PathBuf::from(value);
            }
        }
    }
    PathBuf::from("/tmp")
}

fn set_socket_permissions(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

async fn scan_processes(activities: Activities, activity_tx: broadcast::Sender<String>) {
    let mut timestamps = HashMap::<String, u64>::new();
    let mut pids = HashMap::<String, u32>::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(5));

    loop {
        ticker.tick().await;

        let processes = match tokio::task::spawn_blocking(read_processes).await {
            Ok(processes) => processes,
            Err(error) => {
                eprintln!("[vesk] arRPC: fallo el escaneo de procesos: {error}");
                continue;
            }
        };

        let mut seen = HashSet::new();
        for (pid, path) in processes {
            let Some(application) = match_process(&path) else {
                continue;
            };
            if !seen.insert(application.id.clone()) {
                continue;
            }

            let is_new = !timestamps.contains_key(&application.id);
            let start = *timestamps
                .entry(application.id.clone())
                .or_insert_with(now_ms);
            pids.insert(application.id.clone(), pid);

            if is_new {
                eprintln!("[vesk] arRPC: detectado {}", application.name);
            }

            let activity = normalize_activity(
                &application.id,
                json!({
                    "name": application.name,
                    "timestamps": { "start": start }
                }),
                false,
            );
            publish(
                &activities,
                &activity_tx,
                &format!("process:{}", application.id),
                Some(activity),
                Some(pid),
            );
        }

        let lost = timestamps
            .keys()
            .filter(|id| !seen.contains(*id))
            .cloned()
            .collect::<Vec<_>>();

        for id in lost {
            timestamps.remove(&id);
            let pid = pids.remove(&id);
            let name = detectable()
                .iter()
                .find(|application| application.id == id)
                .map(|application| application.name.as_str())
                .unwrap_or(&id);
            eprintln!("[vesk] arRPC: fin de {name}");
            publish(
                &activities,
                &activity_tx,
                &format!("process:{id}"),
                None,
                pid,
            );
        }
    }
}

fn read_processes() -> Vec<(u32, String)> {
    let mut processes = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return processes;
    };

    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };

        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let end = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
        if end == 0 {
            continue;
        }

        let path = String::from_utf8_lossy(&raw[..end]).to_string();
        processes.push((pid, path));
    }

    processes
}

fn match_process(path: &str) -> Option<&'static Detectable> {
    let candidates = path_candidates(path);

    for application in detectable() {
        for executable in &application.executables {
            if executable.os != "linux" || executable.is_launcher {
                continue;
            }
            if candidates
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(&executable.name))
            {
                return Some(application);
            }
        }
    }

    None
}

fn path_candidates(path: &str) -> Vec<String> {
    let path = path.to_ascii_lowercase().replace('\\', "/");
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    let mut candidates = Vec::new();
    for index in 1..=segments.len() {
        candidates.push(segments[segments.len() - index..].join("/"));
    }

    let variants = candidates
        .iter()
        .flat_map(|candidate| {
            [
                candidate.replace("64", ""),
                candidate.replace(".x64", ""),
                candidate.replace("x64", ""),
                candidate.replace("_64", ""),
            ]
        })
        .collect::<Vec<_>>();

    candidates.extend(variants);
    candidates
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Debug, Deserialize)]
struct Detectable {
    id: String,
    name: String,
    executables: Vec<Executable>,
}

#[derive(Debug, Deserialize)]
struct Executable {
    name: String,
    #[serde(default)]
    os: String,
    #[serde(default)]
    is_launcher: bool,
}

fn detectable() -> &'static [Detectable] {
    static DETECTABLE: OnceLock<Vec<Detectable>> = OnceLock::new();
    DETECTABLE.get_or_init(|| {
        serde_json::from_str(include_str!("../resources/arrpc-linux.json"))
            .expect("la tabla Linux de arRPC debe ser JSON valido")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabla_linux_tiene_entradas() {
        let applications = detectable();
        assert!(!applications.is_empty());
        assert!(applications
            .iter()
            .all(|application| !application.id.is_empty()));
        assert!(applications
            .iter()
            .all(|application| !application.name.is_empty()));
    }

    #[test]
    fn coincide_con_rutas_de_linux() {
        assert_eq!(
            match_process("/home/jugador/Factorio/bin/x64/factorio")
                .map(|application| application.name.as_str()),
            Some("Factorio")
        );
        assert_eq!(
            match_process("/usr/bin/hoi4").map(|application| application.name.as_str()),
            Some("Hearts of Iron IV")
        );
        assert!(match_process("/usr/bin/otro-programa").is_none());
    }

    #[test]
    fn candidatos_incluyen_sufijos_y_variantes() {
        let candidates = path_candidates("/juego/x64/juego.x64.exe");
        assert!(candidates
            .iter()
            .any(|candidate| candidate == "x64/juego.x64.exe"));
        assert!(candidates
            .iter()
            .any(|candidate| candidate == "juego.x64.exe"));
        assert!(candidates.iter().any(|candidate| candidate == "juego.exe"));
    }

    #[test]
    fn normaliza_timestamps_en_segundos() {
        let mut activity = json!({
            "timestamps": { "start": 1_700_000_000u64 }
        });
        normalize_timestamps(&mut activity);
        assert_eq!(
            activity["timestamps"]["start"].as_u64(),
            Some(1_700_000_000_000)
        );

        let mut milliseconds = json!({
            "timestamps": { "start": 1_700_000_000_000u64 }
        });
        normalize_timestamps(&mut milliseconds);
        assert_eq!(
            milliseconds["timestamps"]["start"].as_u64(),
            Some(1_700_000_000_000)
        );
    }

    #[test]
    fn normaliza_actividad_y_botones() {
        let activity = normalize_activity(
            "123",
            json!({
                "name": "Juego",
                "buttons": [
                    { "label": "Web", "url": "https://example.com" }
                ],
                "instance": true
            }),
            true,
        );

        assert_eq!(activity["application_id"], "123");
        assert_eq!(activity["type"], 0);
        assert_eq!(activity["flags"], 1);
        assert_eq!(activity["buttons"], json!(["Web"]));
        assert_eq!(
            activity["metadata"]["button_urls"],
            json!(["https://example.com"])
        );
    }

    #[tokio::test]
    async fn frame_ipc_ida_y_vuelta() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let value = json!({ "cmd": "DISPATCH", "data": { "ok": true } });

        write_frame(&mut writer, IPC_FRAME, &value).await.unwrap();
        let frame = read_frame(&mut reader).await.unwrap().unwrap();

        assert_eq!(frame.0, IPC_FRAME);
        assert_eq!(frame.1, value);
    }

    #[tokio::test]
    async fn frame_invalido_se_rechaza() {
        let (mut writer, mut reader) = tokio::io::duplex(32);
        writer.write_all(&1i32.to_le_bytes()).await.unwrap();
        writer.write_all(&(-1i32).to_le_bytes()).await.unwrap();

        assert!(read_frame(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn ipc_handshake_y_actividad() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let activities: Activities = Arc::new(Mutex::new(HashMap::new()));
        let (activity_tx, mut activity_rx) = broadcast::channel(8);
        let task = tokio::spawn(handle_ipc(server, activities.clone(), activity_tx));

        write_frame(
            &mut client,
            IPC_HANDSHAKE,
            &json!({ "v": 1, "client_id": "123" }),
        )
        .await
        .unwrap();

        let (kind, ready) = read_frame(&mut client).await.unwrap().unwrap();
        assert_eq!(kind, IPC_FRAME);
        assert_eq!(ready["evt"], "READY");

        write_frame(
            &mut client,
            IPC_FRAME,
            &json!({
                "cmd": "SET_ACTIVITY",
                "args": {
                    "pid": 4242,
                    "activity": {
                        "details": "Jugando",
                        "timestamps": { "start": 1_700_000_000u64 }
                    }
                },
                "nonce": "n-1"
            }),
        )
        .await
        .unwrap();

        let (kind, response) = read_frame(&mut client).await.unwrap().unwrap();
        assert_eq!(kind, IPC_FRAME);
        assert_eq!(response["cmd"], "SET_ACTIVITY");
        assert_eq!(response["nonce"], "n-1");
        assert_eq!(response["data"]["application_id"], "123");
        assert_eq!(response["data"]["type"], 0);

        let payload = timeout(Duration::from_secs(2), activity_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["pid"], 4242);
        assert_eq!(payload["activity"]["application_id"], "123");
        assert_eq!(
            payload["activity"]["timestamps"]["start"],
            1_700_000_000_000u64
        );
        assert_eq!(activities.lock().unwrap().len(), 1);

        write_frame(
            &mut client,
            IPC_FRAME,
            &json!({
                "cmd": "SET_ACTIVITY",
                "args": { "pid": 4242, "activity": null },
                "nonce": "n-2"
            }),
        )
        .await
        .unwrap();

        let (_, response) = read_frame(&mut client).await.unwrap().unwrap();
        assert_eq!(response["nonce"], "n-2");
        assert!(response["data"].is_null());

        let clear = timeout(Duration::from_secs(2), activity_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let clear: Value = serde_json::from_str(&clear).unwrap();
        assert!(clear["activity"].is_null());
        assert!(activities.lock().unwrap().is_empty());

        drop(client);
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn bridge_entrega_actividad_y_snapshot() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let activities: Activities = Arc::new(Mutex::new(HashMap::new()));
        let (activity_tx, _) = broadcast::channel(8);
        let task = tokio::spawn(accept_ws(listener, activities.clone(), activity_tx.clone()));

        publish(
            &activities,
            &activity_tx,
            "process:1",
            Some(json!({ "application_id": "1", "name": "Juego" })),
            Some(7),
        );

        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let snapshot = timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let snapshot: Value = serde_json::from_str(snapshot.to_text().unwrap()).unwrap();
        assert_eq!(snapshot["activity"]["name"], "Juego");

        publish(
            &activities,
            &activity_tx,
            "process:1",
            Some(json!({ "application_id": "1", "name": "Juego 2" })),
            Some(7),
        );
        let update = timeout(Duration::from_secs(2), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let update: Value = serde_json::from_str(update.to_text().unwrap()).unwrap();
        assert_eq!(update["activity"]["name"], "Juego 2");

        task.abort();
    }

    #[tokio::test]
    async fn bridge_rechaza_origen_ajeno() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let activities: Activities = Arc::new(Mutex::new(HashMap::new()));
        let (activity_tx, _) = broadcast::channel(8);
        let task = tokio::spawn(accept_ws(listener, activities, activity_tx));

        let mut request =
            tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(
                format!("ws://{address}"),
            )
            .unwrap();
        request.headers_mut().insert(
            "origin",
            tokio_tungstenite::tungstenite::http::HeaderValue::from_static("https://example.com"),
        );

        assert!(tokio_tungstenite::connect_async(request).await.is_err());
        task.abort();
    }

    #[tokio::test]
    async fn arranque_y_parada_del_motor() {
        if enabled() {
            shutdown();
        }

        assert!(set_enabled(true));
        assert!(enabled());
        assert!(!set_enabled(false));
        assert!(!enabled());
    }
}
