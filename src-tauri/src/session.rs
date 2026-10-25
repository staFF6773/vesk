use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
#[cfg(not(all(feature = "cef", target_os = "linux")))]
use std::time::{Instant, SystemTime};

use rusqlite::types::Value;
use rusqlite::{Connection, OpenFlags};

/// Copia de seguridad del token de Discord.
///
/// WebKitGTK guarda el token en el SQLite de localStorage. Este modulo lo lee
/// periodicamente, lo guarda en <app_data>/session.token (0600) y permite
/// reinyectarlo al arrancar antes de que Discord ejecute su JavaScript.
pub struct SessionVault {
    data_dir: PathBuf,
    path: PathBuf,
    enabled: Arc<AtomicBool>,
}

impl SessionVault {
    pub fn new(data_dir: PathBuf) -> Self {
        let path = data_dir.join("session.token");
        Self {
            data_dir,
            path,
            enabled: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn load(&self) -> Option<String> {
        let token = std::fs::read_to_string(&self.path).ok()?;
        let token = token.trim().to_owned();
        (token.len() >= 20).then_some(token)
    }

    pub fn forget(&self) {
        self.enabled.store(false, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.path);
    }

    pub fn capture_now(&self) -> Option<String> {
        if !self.enabled.load(Ordering::Relaxed) {
            return None;
        }

        let token = read_token_from_db(&db_path(&self.data_dir))?;
        let _ = save_token(&self.path, &token);
        Some(token)
    }

    /// Vigila el SQLite de WebKit y actualiza la copia cuando cambia el token.
    ///
    /// Abrir ese SQLite toma un lock compartido que puede retrasar las
    /// escrituras de WebKit (Discord escribe localStorage todo el rato), asi
    /// que solo lo tocamos cuando el archivo cambio de tamano o de fecha.
    ///
    /// Con CEF la sesion vive en el perfil de Chromium: este vigilante no
    /// aplica.
    #[cfg(not(all(feature = "cef", target_os = "linux")))]
    pub fn watch(&self) {
        let data_dir = self.data_dir.clone();
        let path = self.path.clone();
        let enabled = self.enabled.clone();

        std::thread::spawn(move || {
            let db = db_path(&data_dir);
            let mut stamp = None;
            let mut missing_since: Option<Instant> = None;

            loop {
                if !enabled.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_secs(3));
                    continue;
                }

                let current = db_stamp(&db);
                if current != stamp {
                    stamp = current;

                    match read_token_from_db(&db) {
                        Some(token) => {
                            missing_since = None;
                            let saved = std::fs::read_to_string(&path).ok();
                            if saved.as_deref().map(str::trim) != Some(token.as_str()) {
                                let _ = save_token(&path, &token);
                            }
                        }
                        None => {
                            missing_since.get_or_insert_with(Instant::now);
                        }
                    }
                }

                // Si Discord lleva un minuto sin token, la sesion se cerro de
                // verdad: no la resucitamos en el proximo arranque.
                if missing_since.is_some_and(|since| since.elapsed() >= Duration::from_secs(60)) {
                    let _ = std::fs::remove_file(&path);
                    missing_since = None;
                }

                std::thread::sleep(Duration::from_secs(3));
            }
        });
    }
}

fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join("localstorage/https_discord.com_0.localstorage")
}

/// Fecha y tamano del SQLite: cambian cuando WebKit escribe el token.
#[cfg(not(all(feature = "cef", target_os = "linux")))]
fn db_stamp(db: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(db).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

fn read_token_from_db(db: &Path) -> Option<String> {
    if !db.exists() {
        return None;
    }

    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(500));

    let value: Value = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = 'token' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok()?;

    match value {
        Value::Blob(bytes) => decode_token(&bytes),
        Value::Text(text) => sanitize(&text),
        _ => None,
    }
}

fn decode_token(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }

    let text = if bytes.len() % 2 == 0 && bytes.iter().skip(1).step_by(2).all(|byte| *byte == 0) {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    };

    sanitize(&text)
}

fn sanitize(text: &str) -> Option<String> {
    let token = text.trim_end_matches('\0').trim();
    if (20..=4096).contains(&token.len()) && !token.chars().any(char::is_control) {
        Some(token.to_owned())
    } else {
        None
    }
}

fn save_token(path: &Path, token: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, token)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }

    std::fs::rename(tmp, path)
}
