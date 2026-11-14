//! Оверлей поверх игры: прозрачное click-through окно `overlay` со страницей `ui/overlay.html`.
//! Позиция и размер хранятся в `app_data_dir()/overlay.json`. Чистая логика (раскладка, границы экрана,
//! разбор файла) вынесена в функции без tauri и покрыта тестами.
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, PhysicalPosition, PhysicalSize, WebviewUrl, WebviewWindowBuilder};

const LABEL: &str = "overlay";
const DEF_W: u32 = 320;
const DEF_H: u32 = 280;
const MIN_W: u32 = 160;
const MIN_H: u32 = 80;
const MARGIN: i32 = 16;

/// Прямоугольник в физических пикселях.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Сохраняемая геометрия окна оверлея (физические пиксели).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayCfg {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// Позиция по умолчанию: правый верхний угол монитора с отступом.
pub fn default_cfg(screen: Rect) -> OverlayCfg {
    let w = DEF_W.min(screen.w);
    let h = DEF_H.min(screen.h);
    OverlayCfg {
        x: screen.x + screen.w as i32 - w as i32 - MARGIN,
        y: screen.y + MARGIN,
        w,
        h,
    }
}

/// Разбор содержимого overlay.json; мусор или нулевые размеры -> None.
pub fn parse_cfg(text: &str) -> Option<OverlayCfg> {
    let c: OverlayCfg = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    if c.w < MIN_W || c.h < MIN_H || c.w > 10_000 || c.h > 10_000 {
        return None;
    }
    Some(c)
}

pub fn serialize_cfg(c: &OverlayCfg) -> String {
    serde_json::to_string_pretty(c).unwrap_or_else(|_| "{}".into())
}

fn overlap_area(a: &OverlayCfg, m: &Rect) -> i64 {
    let ax2 = a.x as i64 + a.w as i64;
    let ay2 = a.y as i64 + a.h as i64;
    let mx2 = m.x as i64 + m.w as i64;
    let my2 = m.y as i64 + m.h as i64;
    let w = (ax2.min(mx2) - (a.x as i64).max(m.x as i64)).max(0);
    let h = (ay2.min(my2) - (a.y as i64).max(m.y as i64)).max(0);
    w * h
}

/// Приводит сохранённую геометрию к текущим мониторам: если окно (почти) вне экранов
/// (монитор отключили), ставит по умолчанию на первый монитор; иначе зажимает в границы
/// монитора с наибольшим перекрытием. Пустой список мониторов -> конфиг как есть.
pub fn fit_to_screens(c: OverlayCfg, monitors: &[Rect]) -> OverlayCfg {
    let Some(first) = monitors.first() else { return c };
    let best = monitors
        .iter()
        .max_by_key(|m| overlap_area(&c, m))
        .copied()
        .unwrap_or(*first);
    // нужно видеть хотя бы четверть окна, иначе считаем, что оно потеряно
    let visible = overlap_area(&c, &best);
    if visible * 4 < c.w as i64 * c.h as i64 {
        return default_cfg(*first);
    }
    let w = c.w.min(best.w).max(MIN_W.min(best.w));
    let h = c.h.min(best.h).max(MIN_H.min(best.h));
    let max_x = best.x + best.w as i32 - w as i32;
    let max_y = best.y + best.h as i32 - h as i32;
    OverlayCfg {
        x: c.x.clamp(best.x, max_x.max(best.x)),
        y: c.y.clamp(best.y, max_y.max(best.y)),
        w,
        h,
    }
}

fn cfg_path(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok().map(|d| d.join("overlay.json"))
}

fn load_cfg(app: &AppHandle) -> Option<OverlayCfg> {
    let text = std::fs::read_to_string(cfg_path(app)?).ok()?;
    parse_cfg(&text)
}

/// Атомарная запись: во временный файл, затем rename.
fn save_cfg(app: &AppHandle, c: &OverlayCfg) -> Result<(), String> {
    let path = cfg_path(app).ok_or("нет каталога данных приложения")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("не удалось создать каталог: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serialize_cfg(c)).map_err(|e| format!("не удалось записать overlay.json: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("не удалось сохранить overlay.json: {e}"))
}

fn monitors(app: &AppHandle) -> Vec<Rect> {
    app.available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| Rect {
            x: m.position().x,
            y: m.position().y,
            w: m.size().width,
            h: m.size().height,
        })
        .collect()
}

/// Запомнить текущую геометрию окна (ошибки игнорируем).
fn remember(app: &AppHandle, win: &tauri::WebviewWindow) {
    if let (Ok(p), Ok(s)) = (win.outer_position(), win.outer_size()) {
        let c = OverlayCfg { x: p.x, y: p.y, w: s.width, h: s.height };
        if c.w >= MIN_W && c.h >= MIN_H {
            let _ = save_cfg(app, &c);
        }
    }
}

/// Показать/скрыть оверлей. Возвращает true, если после вызова оверлей виден.
#[tauri::command]
pub async fn overlay_toggle(app: AppHandle) -> Result<bool, String> {
    if let Some(win) = app.get_webview_window(LABEL) {
        let visible = win.is_visible().map_err(|e| e.to_string())?;
        if visible {
            remember(&app, &win);
            win.hide().map_err(|e| e.to_string())?;
            return Ok(false);
        }
        win.show().map_err(|e| e.to_string())?;
        let _ = win.set_ignore_cursor_events(true);
        let _ = win.set_always_on_top(true);
        return Ok(true);
    }

    let mons = monitors(&app);
    let primary = mons.first().copied().unwrap_or(Rect { x: 0, y: 0, w: 1920, h: 1080 });
    let cfg = match load_cfg(&app) {
        Some(c) => fit_to_screens(c, &mons),
        None => default_cfg(primary),
    };

    let win = WebviewWindowBuilder::new(&app, LABEL, WebviewUrl::App("overlay.html".into()))
        .title("hollow++ overlay")
        .transparent(true)
        .decorations(false)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .focused(false)
        .visible(false)
        .build()
        .map_err(|e| format!("не удалось создать окно оверлея: {e}"))?;
    let _ = win.set_size(PhysicalSize::new(cfg.w, cfg.h));
    let _ = win.set_position(PhysicalPosition::new(cfg.x, cfg.y));
    let _ = win.set_ignore_cursor_events(true);
    win.show().map_err(|e| e.to_string())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S1: Rect = Rect { x: 0, y: 0, w: 1920, h: 1080 };
    const S2: Rect = Rect { x: 1920, y: 0, w: 1280, h: 1024 };

    #[test]
    fn default_in_top_right() {
        let c = default_cfg(S1);
        assert_eq!(c, OverlayCfg { x: 1920 - 320 - 16, y: 16, w: 320, h: 280 });
    }

    #[test]
    fn default_on_second_monitor_offset() {
        let c = default_cfg(S2);
        assert_eq!(c.x, 1920 + 1280 - 320 - 16);
    }

    #[test]
    fn default_shrinks_on_tiny_screen() {
        let c = default_cfg(Rect { x: 0, y: 0, w: 200, h: 100 });
        assert_eq!((c.w, c.h), (200, 100));
    }

    #[test]
    fn parse_roundtrip_and_garbage() {
        let c = OverlayCfg { x: -5, y: 10, w: 300, h: 200 };
        assert_eq!(parse_cfg(&serialize_cfg(&c)), Some(c));
        assert_eq!(parse_cfg("не json"), None);
        assert_eq!(parse_cfg(r#"{"x":0,"y":0,"w":0,"h":0}"#), None);
        assert_eq!(parse_cfg(r#"{"x":0,"y":0,"w":99999,"h":200}"#), None);
        assert_eq!(parse_cfg("\u{feff}{\"x\":1,\"y\":2,\"w\":300,\"h\":200}").map(|c| c.x), Some(1));
    }

    #[test]
    fn inside_screen_unchanged() {
        let c = OverlayCfg { x: 100, y: 100, w: 320, h: 280 };
        assert_eq!(fit_to_screens(c, &[S1]), c);
    }

    #[test]
    fn lost_monitor_falls_back_to_default() {
        let c = OverlayCfg { x: 2500, y: 100, w: 320, h: 280 };
        assert_eq!(fit_to_screens(c, &[S1]), default_cfg(S1));
    }

    #[test]
    fn partially_offscreen_is_clamped() {
        let c = OverlayCfg { x: 1800, y: 1000, w: 320, h: 280 };
        // видно 120x80 из 320x280: меньше четверти -> по умолчанию
        assert_eq!(fit_to_screens(c, &[S1]), default_cfg(S1));
        let c = OverlayCfg { x: 1700, y: 900, w: 320, h: 280 };
        let f = fit_to_screens(c, &[S1]);
        assert_eq!((f.x, f.y), (1600, 800));
    }

    #[test]
    fn negative_origin_clamped_to_monitor() {
        let c = OverlayCfg { x: -100, y: -50, w: 320, h: 280 };
        let f = fit_to_screens(c, &[S1]);
        assert_eq!((f.x, f.y), (0, 0));
    }

    #[test]
    fn picks_monitor_with_most_overlap() {
        let c = OverlayCfg { x: 2000, y: 50, w: 320, h: 280 };
        assert_eq!(fit_to_screens(c, &[S1, S2]), c);
    }

    #[test]
    fn oversize_window_shrinks_to_monitor() {
        let c = OverlayCfg { x: 0, y: 0, w: 2000, h: 1200 };
        let f = fit_to_screens(c, &[S1]);
        assert_eq!((f.w, f.h), (1920, 1080));
    }

    #[test]
    fn no_monitors_keeps_cfg() {
        let c = OverlayCfg { x: 5, y: 5, w: 300, h: 200 };
        assert_eq!(fit_to_screens(c, &[]), c);
    }
}
