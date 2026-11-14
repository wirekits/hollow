//! Названия предметов Rust по itemId. Rust+ отдаёт в магазинах только числовые id, а таблицы в игре нет.
//! Берём items.json из rustplusplus ({"<id>": {"shortname","name",...}}, ~1200 предметов) при запуске,
//! кэшируем в `<данные приложения>/items.json` и обновляем раз в неделю. В репозиторий файл не кладём.
//! Нет сети и кэша — работают встроенные базовые названия (forms::item_name).

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Manager};

const URL: &str = "https://raw.githubusercontent.com/alexemanuelol/rustplusplus/master/src/staticFiles/items.json";
const REFRESH: Duration = Duration::from_secs(7 * 24 * 3600);

static NAMES: RwLock<Option<HashMap<i32, String>>> = RwLock::new(None);

/// Разбор items.json (чистая функция): ключ — id строкой (бывает отрицательный), значение — {name}.
pub fn parse(text: &str) -> HashMap<i32, String> {
    let v: serde_json::Value = match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    v.as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, x)| {
                    let id: i32 = k.parse().ok()?;
                    let n = x.get("name")?.as_str()?.trim();
                    (!n.is_empty()).then(|| (id, n.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn name(id: i32) -> Option<String> {
    NAMES.read().ok()?.as_ref()?.get(&id).cloned()
}

fn set(m: HashMap<i32, String>) {
    if m.len() > 100 {
        if let Ok(mut g) = NAMES.write() {
            *g = Some(m);
        }
    }
}

/// Загрузить кэш и при необходимости обновить из сети (в фоне, ошибки только в лог).
pub async fn init(app: &AppHandle) {
    let Some(path) = app.path().app_data_dir().ok().map(|d| d.join("items.json")) else { return };
    let mut fresh = false;
    if let Ok(t) = std::fs::read_to_string(&path) {
        set(parse(&t));
        fresh = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .map_or(false, |age| age < REFRESH);
    }
    if fresh {
        return;
    }
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(20)).user_agent("hollow-plus").build() {
        Ok(c) => c,
        Err(_) => return,
    };
    match client.get(URL).send().await {
        Ok(r) if r.status().is_success() => match r.text().await {
            Ok(t) => {
                let m = parse(&t);
                if m.len() > 100 {
                    let tmp = path.with_extension("json.tmp");
                    if std::fs::write(&tmp, &t).is_ok() {
                        let _ = std::fs::rename(&tmp, &path);
                    }
                    log::info!("названия предметов обновлены: {}", m.len());
                    set(m);
                }
            }
            Err(e) => log::warn!("названия предметов: {e}"),
        },
        Ok(r) => log::warn!("названия предметов: HTTP {}", r.status()),
        Err(_) => log::warn!("названия предметов: нет сети, работаем с кэшем"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_shape() {
        let t = r#"{"-1843426638":{"shortname":"ammo.rocket.mlrs","name":"MLRS Rocket","description":"x"},
                    "1248356124":{"shortname":"explosive.timed","name":"Timed Explosive Charge"},
                    "bad":{"name":"x"},"5":{"shortname":"y"}}"#;
        let m = parse(t);
        assert_eq!(m.get(&-1843426638).map(String::as_str), Some("MLRS Rocket"));
        assert_eq!(m.get(&1248356124).map(String::as_str), Some("Timed Explosive Charge"));
        assert_eq!(m.len(), 2);
        assert!(parse("не json").is_empty());
    }
}
