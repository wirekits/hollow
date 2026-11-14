//! Оболочка приложения: трей, закрытие в трей, автозапуск, нативные уведомления, настройки приложения.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_notification::NotificationExt as _;

const MAIN: &str = "main";
const FILE: &str = "settings.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub autostart: bool,
    pub close_to_tray: bool,
    pub start_minimized: bool,
    pub native_notify: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self { autostart: false, close_to_tray: true, start_minimized: false, native_notify: true }
    }
}

/// Состояние оболочки (manage() делается в `setup`).
pub struct ShellState {
    settings: Mutex<AppSettings>,
    path: PathBuf,
}

// ---------------------------------------------------------------- чистая логика

/// Применяет патч (JSON-объект с булевыми полями) к настройкам. Неизвестные поля игнорируются,
/// небулевое значение известного поля - ошибка.
pub fn merge_patch(cur: &AppSettings, patch: &serde_json::Value) -> Result<AppSettings, String> {
    let obj = patch.as_object().ok_or_else(|| "патч настроек должен быть объектом".to_string())?;
    let mut out = cur.clone();
    for (k, v) in obj {
        let slot = match k.as_str() {
            "autostart" => &mut out.autostart,
            "close_to_tray" => &mut out.close_to_tray,
            "start_minimized" => &mut out.start_minimized,
            "native_notify" => &mut out.native_notify,
            _ => continue,
        };
        *slot = v.as_bool().ok_or_else(|| format!("поле «{k}» должно быть true/false"))?;
    }
    Ok(out)
}

/// Читает настройки; битый/отсутствующий файл -> значения по умолчанию.
pub fn load_settings(path: &Path) -> AppSettings {
    std::fs::read(path)
        .ok()
        .and_then(|b| {
            let b = b.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(&b).to_vec();
            serde_json::from_slice(&b).ok()
        })
        .unwrap_or_default()
}

/// Атомарная запись: во временный файл рядом, затем rename.
pub fn save_settings(path: &Path, s: &AppSettings) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Windows: rename поверх существующего иногда падает
        let _ = std::fs::remove_file(path);
        std::fs::rename(&tmp, path).map_err(|_| e.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------- окно

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(MAIN) {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn toggle_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(MAIN) {
        if w.is_visible().unwrap_or(false) && !w.is_minimized().unwrap_or(false) {
            let _ = w.hide();
        } else {
            show_main(app);
        }
    }
}

fn current(app: &AppHandle) -> AppSettings {
    match app.try_state::<ShellState>() {
        Some(st) => st.settings.lock().map(|g| g.clone()).unwrap_or_default(),
        None => AppSettings::default(),
    }
}

/// Нативное уведомление Windows. Не показывается, если выключено в настройках или главное окно в фокусе.
pub fn native_notify(app: &AppHandle, title: &str, text: &str) {
    if !current(app).native_notify {
        return;
    }
    if let Some(w) = app.get_webview_window(MAIN) {
        if w.is_focused().unwrap_or(false) {
            return;
        }
    }
    let title = if title.trim().is_empty() { "hollow++" } else { title };
    let _ = app.notification().builder().title(title).body(text).show();
}

/// Вызывается из lib.rs в setup: состояние, трей, обработчик закрытия окна.
pub fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let path = dir.join(FILE);
    let settings = load_settings(&path);
    let start_hidden = settings.start_minimized;
    app.manage(ShellState { settings: Mutex::new(settings), path });

    // трей
    let open = MenuItem::with_id(app, "open", "Открыть", true, None::<&str>)?;
    let overlay = MenuItem::with_id(app, "overlay", "Оверлей", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &overlay, &sep, &quit])?;
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("hollow++")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id().as_ref() {
            "open" => show_main(app),
            "overlay" => {
                let h = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = crate::overlay::overlay_toggle(h).await;
                });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                toggle_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;

    // главное окно: закрытие в трей, старт свёрнутым (окно в конфиге visible=false)
    if let Some(w) = app.get_webview_window(MAIN) {
        let handle = app.handle().clone();
        w.on_window_event(move |ev| {
            if let WindowEvent::CloseRequested { api, .. } = ev {
                if current(&handle).close_to_tray {
                    api.prevent_close();
                    if let Some(w) = handle.get_webview_window(MAIN) {
                        let _ = w.hide();
                    }
                }
            }
        });
        if !start_hidden {
            let _ = w.show();
        }
    }
    Ok(())
}

/// Поднимает главное окно (для single-instance).
pub fn raise_main(app: &AppHandle) {
    show_main(app);
}

// ---------------------------------------------------------------- команды

#[tauri::command]
pub async fn app_settings_get(app: AppHandle) -> Result<AppSettings, String> {
    let mut s = current(&app);
    if let Ok(on) = app.autolaunch().is_enabled() {
        s.autostart = on;
    }
    Ok(s)
}

#[tauri::command]
pub async fn app_settings_set(app: AppHandle, patch: serde_json::Value) -> Result<AppSettings, String> {
    let st = app.try_state::<ShellState>().ok_or_else(|| "оболочка не инициализирована".to_string())?;
    let (old, new) = {
        let g = st.settings.lock().map_err(|_| "блокировка настроек".to_string())?;
        (g.clone(), merge_patch(&g, &patch)?)
    };
    if new.autostart != old.autostart || patch.get("autostart").is_some() {
        let al = app.autolaunch();
        let r = if new.autostart { al.enable() } else { al.disable() };
        r.map_err(|e| format!("не удалось изменить автозапуск: {e}"))?;
    }
    save_settings(&st.path, &new)?;
    *st.settings.lock().map_err(|_| "блокировка настроек".to_string())? = new.clone();
    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merge_applies_known_fields() {
        let s = merge_patch(&AppSettings::default(), &json!({"autostart": true, "close_to_tray": false})).unwrap();
        assert!(s.autostart && !s.close_to_tray && !s.start_minimized && s.native_notify);
    }

    #[test]
    fn merge_ignores_unknown_and_rejects_bad_types() {
        let d = AppSettings::default();
        assert_eq!(merge_patch(&d, &json!({"zzz": 1})).unwrap(), d);
        assert!(merge_patch(&d, &json!({"autostart": "yes"})).is_err());
        assert!(merge_patch(&d, &json!([1])).is_err());
    }

    #[test]
    fn defaults_for_partial_json() {
        let s: AppSettings = serde_json::from_str(r#"{"autostart":true}"#).unwrap();
        assert!(s.autostart && s.close_to_tray && s.native_notify);
    }

    #[test]
    fn save_load_roundtrip_and_bad_file() {
        let dir = std::env::temp_dir().join(format!("hollow-shell-test-{}", std::process::id()));
        let p = dir.join("settings.json");
        assert_eq!(load_settings(&p), AppSettings::default());
        let s = AppSettings { autostart: true, close_to_tray: false, start_minimized: true, native_notify: false };
        save_settings(&p, &s).unwrap();
        save_settings(&p, &s).unwrap();
        assert_eq!(load_settings(&p), s);
        std::fs::write(&p, b"{broken").unwrap();
        assert_eq!(load_settings(&p), AppSettings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
