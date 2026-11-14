//! Автоматизация: ночное выключение групп устройств, ручное управление группами.
use crate::types::Alert;
use chrono::Timelike;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Manager};

const TICK: Duration = Duration::from_secs(20);
const DEVICE_PAUSE: Duration = Duration::from_millis(150);

#[derive(Default)]
pub struct AutomationState {
    task: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

// ---------------------------------------------------------------- чистая логика

/// "HH:MM" -> минуты от полуночи (0..1439).
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

/// Попадает ли момент `now` (минуты от полуночи) в окно [from, to). Окно может идти через полночь.
/// from == to трактуется как пустое окно.
pub fn in_night_window(now: u32, from: u32, to: u32) -> bool {
    if from == to {
        false
    } else if from < to {
        now >= from && now < to
    } else {
        now >= from || now < to
    }
}

/// Шаг автомата: (нужно ли гасить сейчас, новое значение флага «уже гасили в этом окне»).
pub fn night_step(inside: bool, done: bool) -> (bool, bool) {
    if inside {
        (!done, true)
    } else {
        (false, false)
    }
}

/// Уникальные id устройств выбранных групп, порядок сохраняется.
pub fn collect_night_devices(cfg: &crate::types::AutomationCfg) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for gid in &cfg.night_groups {
        if let Some(g) = cfg.groups.iter().find(|g| &g.id == gid) {
            for d in &g.device_ids {
                let d = d.trim().to_string();
                if !d.is_empty() && !out.contains(&d) {
                    out.push(d);
                }
            }
        }
    }
    out
}

fn local_minutes() -> u32 {
    let n = chrono::Local::now();
    n.hour() * 60 + n.minute()
}

// ---------------------------------------------------------------- задача

fn active_cfg(app: &AppHandle) -> Option<crate::types::AutomationCfg> {
    app.state::<crate::profiles::ProfilesState>()
        .active_profile()
        .map(|p| p.automation)
}

async fn tick(app: &AppHandle, done: &mut bool) {
    let cfg = match active_cfg(app) {
        Some(c) if c.night_off => c,
        _ => {
            *done = false;
            return;
        }
    };
    let (from, to) = match (parse_hhmm(&cfg.night_from), parse_hhmm(&cfg.night_to)) {
        (Some(f), Some(t)) => (f, t),
        _ => {
            *done = false;
            return;
        }
    };
    let inside = in_night_window(local_minutes(), from, to);
    let (act, new_done) = night_step(inside, *done);
    *done = new_done;
    if !act {
        return;
    }
    let ids = collect_night_devices(&cfg);
    if ids.is_empty() {
        return;
    }
    let total = ids.len();
    let mut ok = 0usize;
    for id in ids {
        if crate::rustplus::set_device(app.clone(), id, false).await.is_ok() {
            ok += 1;
        }
        tokio::time::sleep(DEVICE_PAUSE).await;
    }
    crate::notify::dispatch(
        app,
        Alert {
            kind: "info".into(),
            prio: "low".into(),
            title: "Ночной режим".into(),
            text: format!("Выключено устройств: {} из {}", ok, total),
        },
    )
    .await;
}

pub async fn start(app: &AppHandle) {
    stop(app).await;
    let app2 = app.clone();
    let handle = tauri::async_runtime::spawn(async move {
        let mut done = false;
        loop {
            tick(&app2, &mut done).await;
            tokio::time::sleep(TICK).await;
        }
    });
    let st = app.state::<AutomationState>();
    let mut g = match st.task.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    *g = Some(handle);
}

pub async fn stop(app: &AppHandle) {
    let st = app.state::<AutomationState>();
    let h = {
        let mut g = match st.task.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.take()
    };
    if let Some(h) = h {
        h.abort();
    }
}

#[tauri::command]
pub async fn group_set(app: AppHandle, id: String, on: bool) -> Result<(), String> {
    let cfg = active_cfg(&app).ok_or_else(|| "нет активной пачки".to_string())?;
    let group = cfg
        .groups
        .iter()
        .find(|g| g.id == id)
        .ok_or_else(|| "группа не найдена".to_string())?;
    let ids: Vec<String> = group.device_ids.clone();
    if ids.is_empty() {
        return Err("в группе нет устройств".into());
    }
    let total = ids.len();
    let mut failed = 0usize;
    for (i, d) in ids.into_iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(DEVICE_PAUSE).await;
        }
        if crate::rustplus::set_device(app.clone(), d, on).await.is_err() {
            failed += 1;
        }
    }
    if failed == 0 {
        Ok(())
    } else {
        Err(format!("не удалось переключить устройств: {} из {}", failed, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AutomationCfg, DeviceGroup};

    #[test]
    fn hhmm() {
        assert_eq!(parse_hhmm("22:00"), Some(1320));
        assert_eq!(parse_hhmm(" 7:05 "), Some(425));
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("23:59"), Some(1439));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("12:60"), None);
        assert_eq!(parse_hhmm("abc"), None);
        assert_eq!(parse_hhmm(""), None);
    }

    #[test]
    fn window_same_day() {
        let (f, t) = (9 * 60, 17 * 60);
        assert!(!in_night_window(8 * 60 + 59, f, t));
        assert!(in_night_window(9 * 60, f, t));
        assert!(in_night_window(16 * 60 + 59, f, t));
        assert!(!in_night_window(17 * 60, f, t));
    }

    #[test]
    fn window_over_midnight() {
        let (f, t) = (22 * 60, 7 * 60);
        assert!(!in_night_window(21 * 60 + 59, f, t));
        assert!(in_night_window(22 * 60, f, t));
        assert!(in_night_window(23 * 60 + 59, f, t));
        assert!(in_night_window(0, f, t));
        assert!(in_night_window(6 * 60 + 59, f, t));
        assert!(!in_night_window(7 * 60, f, t));
        assert!(!in_night_window(12 * 60, f, t));
    }

    #[test]
    fn window_empty_when_equal() {
        assert!(!in_night_window(0, 600, 600));
        assert!(!in_night_window(600, 600, 600));
    }

    #[test]
    fn step_once_per_window() {
        assert_eq!(night_step(false, false), (false, false));
        assert_eq!(night_step(true, false), (true, true));
        assert_eq!(night_step(true, true), (false, true));
        assert_eq!(night_step(false, true), (false, false));
    }

    #[test]
    fn night_devices_unique() {
        let g = |id: &str, d: &[&str]| DeviceGroup {
            id: id.into(),
            name: id.into(),
            device_ids: d.iter().map(|s| s.to_string()).collect(),
        };
        let cfg = AutomationCfg {
            night_off: true,
            night_groups: vec!["a".into(), "b".into(), "zzz".into()],
            groups: vec![g("a", &["1", "2", " "]), g("b", &["2", "3"]), g("c", &["9"])],
            ..Default::default()
        };
        assert_eq!(collect_night_devices(&cfg), vec!["1", "2", "3"]);
    }
}
