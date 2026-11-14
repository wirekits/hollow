//! Автообновление через GitHub Releases (wirekits/hollow, latest.json). Подпись проверяется
//! публичным ключом из tauri.conf.json; приватный ключ есть только в секретах GitHub Actions.
//! Проверка через 15 с после запуска и раз в 6 часов. Найдено обновление: событие `update_available`,
//! установка только по кнопке в интерфейсе.

use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::{Update, UpdaterExt};

static PENDING: Mutex<Option<Update>> = Mutex::new(None);
const EVERY: Duration = Duration::from_secs(6 * 3600);

fn info(app: &AppHandle, u: Option<&Update>) -> serde_json::Value {
    serde_json::json!({
        "current": app.package_info().version.to_string(),
        "available": u.map(|u| u.version.clone()),
        "notes": u.and_then(|u| u.body.clone()).unwrap_or_default(),
    })
}

async fn check(app: &AppHandle) -> Result<serde_json::Value, String> {
    let up = app.updater().map_err(|e| e.to_string())?;
    let found = up.check().await.map_err(|e| e.to_string())?;
    let v = info(app, found.as_ref());
    if let Some(u) = &found {
        log::info!("доступно обновление {}", u.version);
        let _ = app.emit("update_available", &v);
    }
    if let Ok(mut g) = PENDING.lock() {
        *g = found;
    }
    Ok(v)
}

pub fn start(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(15)).await;
        loop {
            if let Err(e) = check(&app).await {
                log::warn!("проверка обновлений: {e}");
            }
            tokio::time::sleep(EVERY).await;
        }
    });
}

#[tauri::command]
pub async fn update_check(app: AppHandle) -> Result<serde_json::Value, String> {
    check(&app).await.map_err(|e| format!("не удалось проверить обновления: {e}"))
}

/// Скачать, установить (NSIS в тихом режиме) и перезапустить.
#[tauri::command]
pub async fn update_install(app: AppHandle) -> Result<(), String> {
    let u = PENDING.lock().ok().and_then(|mut g| g.take()).ok_or("обновлений нет")?;
    log::info!("установка обновления {}", u.version);
    u.download_and_install(|_, _| {}, || {}).await.map_err(|e| {
        log::warn!("установка обновления: {e}");
        format!("не удалось установить обновление: {e}")
    })?;
    app.restart();
}
