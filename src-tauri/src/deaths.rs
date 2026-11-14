//! Смерти: свои (push Rust+ «You were killed by X», см. pairing/notif.rs) с ником и SteamID убийцы
//! и квадратом из последнего снимка команды. История — `deaths.json` в каталоге данных (последние 50).
//! Смерти тимейтов алертит rustplus/alerts.rs (diff_team), на карте их рисует фронт по team_info.

use crate::pairing::notif::{killer_from_title, Death};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

const MAX: usize = 50;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DeathRec {
    pub at: i64,
    /// Ник убийцы (из заголовка push) или сам заголовок, если ник не распознан.
    pub killer: String,
    #[serde(default)]
    pub killer_id: String,
    #[serde(default)]
    pub grid: String,
    #[serde(default)]
    pub server: String,
}

fn path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("deaths.json"))
}

fn load(app: &AppHandle) -> Vec<DeathRec> {
    path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_default()
}

fn save(app: &AppHandle, list: &[DeathRec]) {
    let Some(p) = path(app) else { return };
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_string(list).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

/// Запись из push (чистая функция): ник из заголовка, иначе заголовок, иначе «неизвестно».
pub fn record_from(d: &Death, grid: Option<String>, at: i64) -> DeathRec {
    let killer = killer_from_title(&d.title)
        .or_else(|| (!d.title.is_empty()).then(|| d.title.clone()))
        .unwrap_or_else(|| "неизвестно".into());
    DeathRec { at, killer, killer_id: d.killer_id.clone(), grid: grid.unwrap_or_default(), server: d.server.clone() }
}

/// Добавить в начало истории, обрезать до MAX.
pub fn push(list: &mut Vec<DeathRec>, r: DeathRec) {
    list.insert(0, r);
    list.truncate(MAX);
}

pub async fn on_death(app: &AppHandle, d: Death) {
    let me = app
        .try_state::<crate::profiles::ProfilesState>()
        .and_then(|s| s.active_server())
        .map(|c| c.player_id)
        .unwrap_or_default();
    let grid = if me.is_empty() { None } else { crate::rustplus::member_grid(&me) };
    let now = chrono::Utc::now().timestamp_millis();
    let r = record_from(&d, grid, now);
    log::info!("смерть: убийца «{}», квадрат «{}»", r.killer, r.grid);
    let mut list = load(app);
    push(&mut list, r.clone());
    save(app, &list);
    let _ = app.emit("deaths", &list);
    let text = if r.grid.is_empty() { format!("Убил {}", r.killer) } else { format!("Убил {} · {}", r.killer, r.grid) };
    crate::notify::dispatch(
        app,
        crate::types::Alert { kind: "death".into(), prio: "high".into(), title: "Тебя убили".into(), text },
    )
    .await;
}

#[tauri::command]
pub async fn deaths_list(app: AppHandle) -> Result<Vec<DeathRec>, String> {
    Ok(load(&app))
}

#[tauri::command]
pub async fn deaths_clear(app: AppHandle) -> Result<(), String> {
    save(&app, &[]);
    let _ = app.emit("deaths", Vec::<DeathRec>::new());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(title: &str, id: &str) -> Death {
        Death { title: title.into(), killer_id: id.into(), server: "S".into() }
    }

    #[test]
    fn killer_name_and_fallbacks() {
        let r = record_from(&d("You were killed by John Silver", "7656"), Some("G12".into()), 5);
        assert_eq!((r.killer.as_str(), r.killer_id.as_str(), r.grid.as_str(), r.at), ("John Silver", "7656", "G12", 5));
        assert_eq!(record_from(&d("You died", ""), None, 0).killer, "You died");
        assert_eq!(record_from(&d("", ""), None, 0).killer, "неизвестно");
    }

    #[test]
    fn history_capped_newest_first() {
        let mut l = Vec::new();
        for i in 0..60 {
            push(&mut l, DeathRec { at: i, ..Default::default() });
        }
        assert_eq!(l.len(), MAX);
        assert_eq!(l[0].at, 59);
    }
}
