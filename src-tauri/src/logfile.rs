//! Лог-файл для диагностики: `<данные приложения>/logs/hollow.log`, при 2 МБ переезжает в hollow.old.log.
//! Пишется через крейт `log` (log::info!/warn!/error!). Секреты (токены, пароли, ключи) сюда не пишем:
//! вызывающий код передаёт только состояния и тексты ошибок, уже очищенные от ключей.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

const MAX_BYTES: u64 = 2 * 1024 * 1024;

struct FileLog {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

impl FileLog {
    fn open(&self) -> Option<File> {
        OpenOptions::new().create(true).append(true).open(&self.path).ok()
    }

    fn rotate_if_big(&self, f: &mut Option<File>) {
        let big = f.as_ref().and_then(|x| x.metadata().ok()).map_or(false, |m| m.len() > MAX_BYTES);
        if big {
            *f = None;
            let _ = std::fs::rename(&self.path, self.path.with_file_name("hollow.old.log"));
            *f = self.open();
        }
    }
}

/// Строка лога (чистая функция): время, уровень, модуль, текст в одну строку.
pub fn line(ts: &str, level: log::Level, target: &str, msg: &str) -> String {
    let module = target.strip_prefix("hollow_lib::").unwrap_or(target);
    format!("{ts} {:<5} {module}: {}\n", level, msg.replace(['\r', '\n'], " "))
}

impl log::Log for FileLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        // свои модули с info, чужие крейты только warn и выше (иначе steam-vent/tungstenite засыпают лог)
        m.level() <= if m.target().starts_with("hollow_lib") { log::Level::Info } else { log::Level::Warn }
    }

    fn log(&self, r: &log::Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let s = line(&ts, r.level(), r.target(), &r.args().to_string());
        if let Ok(mut g) = self.file.lock() {
            if g.is_none() {
                *g = self.open();
            }
            self.rotate_if_big(&mut g);
            if let Some(f) = g.as_mut() {
                let _ = f.write_all(s.as_bytes());
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut g) = self.file.lock() {
            if let Some(f) = g.as_mut() {
                let _ = f.flush();
            }
        }
    }
}

pub fn dir(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("logs")
}

/// Включить запись в файл. Повторный вызов ничего не делает.
pub fn init(data_dir: &std::path::Path) {
    let d = dir(data_dir);
    let _ = std::fs::create_dir_all(&d);
    let logger = FileLog { path: d.join("hollow.log"), file: Mutex::new(None) };
    if log::set_logger(Box::leak(Box::new(logger))).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
        log::info!("запуск hollow++ {}", env!("CARGO_PKG_VERSION"));
    }
}

/// Открыть папку с логами в проводнике.
#[tauri::command]
pub async fn log_open(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    let d = dir(&app.path().app_data_dir().map_err(|e| e.to_string())?);
    let _ = std::fs::create_dir_all(&d);
    std::process::Command::new("explorer").arg(&d).spawn().map_err(|e| format!("не удалось открыть папку: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_and_short_module() {
        let s = line("2026-11-14 15:00:00", log::Level::Warn, "hollow_lib::rustplus", "a\nb");
        assert_eq!(s, "2026-11-14 15:00:00 WARN  rustplus: a b\n");
    }
}
