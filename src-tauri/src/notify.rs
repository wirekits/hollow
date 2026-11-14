//! Уведомления: приложение / тим-чат / Discord webhook / Telegram.
//! Секреты (webhook, tg_token) не логируются и не попадают в тексты ошибок.
use crate::types::{Alert, NotifyCfg};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

pub const DISCORD_LIMIT: usize = 2000;
pub const TELEGRAM_LIMIT: usize = 4096;
pub const GAME_CHAT_LIMIT: usize = 128;
pub const DEDUP_WINDOW_MS: u64 = 30_000;
const NET_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RETRY_WAIT_MS: u64 = 5_000;

// ---------------------------------------------------------------- чистая логика

/// low=0, med=1, high=2; пусто/неизвестное = low.
pub fn prio_rank(p: &str) -> u8 {
    match p.trim().to_lowercase().as_str() {
        "high" => 2,
        "med" | "medium" => 1,
        _ => 0,
    }
}

/// Проходит ли алерт порог min_prio.
pub fn passes_min_prio(alert_prio: &str, min_prio: &str) -> bool {
    prio_rank(alert_prio) >= prio_rank(min_prio)
}

/// Обрезка по числу символов (не байт) с многоточием.
pub fn truncate_chars(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    if limit == 0 {
        return String::new();
    }
    if limit == 1 {
        return "…".to_string();
    }
    let mut out: String = s.chars().take(limit - 1).collect();
    out.push('…');
    out
}

pub fn format_discord(a: &Alert) -> String {
    let body = if a.title.trim().is_empty() {
        a.text.clone()
    } else {
        format!("**{}**\n{}", a.title, a.text)
    };
    truncate_chars(&body, DISCORD_LIMIT)
}

pub fn format_telegram(a: &Alert) -> String {
    let body = if a.title.trim().is_empty() {
        a.text.clone()
    } else {
        format!("{}\n{}", a.title, a.text)
    };
    truncate_chars(&body, TELEGRAM_LIMIT)
}

pub fn format_game_chat(a: &Alert) -> String {
    let flat = a.text.replace(['\n', '\r'], " ");
    let body = if a.title.trim().is_empty() {
        flat
    } else {
        format!("[{}] {}", a.title, flat)
    };
    truncate_chars(&body, GAME_CHAT_LIMIT)
}

fn dedup_key(a: &Alert) -> String {
    format!("{}\u{1}{}\u{1}{}", a.kind, a.title, a.text)
}

/// Дедуп: true = слать (и запоминает), false = дубль в окне. Старые записи вычищаются.
pub fn dedup_should_send(
    seen: &mut HashMap<String, u64>,
    key: &str,
    now_ms: u64,
    window_ms: u64,
) -> bool {
    seen.retain(|_, t| now_ms.saturating_sub(*t) < window_ms);
    if seen.contains_key(key) {
        return false;
    }
    seen.insert(key.to_string(), now_ms);
    true
}

/// Разбор Retry-After (секунды, возможно дробные) в мс с ограничением сверху.
pub fn parse_retry_after_ms(v: &str) -> Option<u64> {
    let f: f64 = v.trim().parse().ok()?;
    if !f.is_finite() || f < 0.0 {
        return None;
    }
    Some(((f * 1000.0) as u64).min(MAX_RETRY_WAIT_MS))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn seen_store() -> &'static Mutex<HashMap<String, u64>> {
    static S: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn clean(opt: &Option<String>) -> Option<String> {
    opt.as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------- сеть

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(NET_TIMEOUT)
        .build()
        .map_err(|_| "не удалось создать HTTP-клиент".to_string())
}

/// Текст ошибки без URL (в нём токен).
fn net_err(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "таймаут запроса (10 с)".into()
    } else if e.is_connect() {
        "нет соединения с сервером".into()
    } else {
        "ошибка сети".into()
    }
}

async fn post_json(
    c: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
    service: &str,
) -> Result<(), String> {
    let mut retried = false;
    loop {
        let resp = c
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|e| format!("{}: {}", service, net_err(&e)))?;
        let st = resp.status();
        if st.is_success() {
            return Ok(());
        }
        if st.as_u16() == 429 && !retried {
            retried = true;
            let hdr = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after_ms);
            tokio::time::sleep(Duration::from_millis(hdr.unwrap_or(1000))).await;
            continue;
        }
        return Err(match st.as_u16() {
            429 => format!("{}: слишком много запросов (429), попробуйте позже", service),
            400 => format!("{}: сервис отклонил запрос (400), проверьте настройки", service),
            401 | 403 | 404 => format!(
                "{}: доступ отклонён или адрес/токен неверен ({})",
                service,
                st.as_u16()
            ),
            c => format!("{}: ошибка HTTP {}", service, c),
        });
    }
}

async fn send_discord(webhook: &str, a: &Alert) -> Result<(), String> {
    if !webhook.starts_with("https://") {
        return Err("Discord: webhook должен начинаться с https://".into());
    }
    let body = serde_json::json!({ "content": format_discord(a), "username": "hollow++" });
    post_json(&client()?, webhook, &body, "Discord").await
}

async fn send_tg(token: &str, chat: &str, a: &Alert) -> Result<(), String> {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
    let body = serde_json::json!({ "chat_id": chat, "text": format_telegram(a) });
    post_json(&client()?, &url, &body, "Telegram").await
}

fn emit_app(app: &AppHandle, a: &Alert) {
    let _ = app.emit(
        "alert",
        serde_json::json!({
            "t": now_ms(),
            "kind": a.kind,
            "prio": a.prio,
            "title": a.title,
            "text": a.text,
        }),
    );
}

// ---------------------------------------------------------------- API

pub async fn dispatch(app: &AppHandle, alert: Alert) {
    let cfg: NotifyCfg = match app
        .state::<crate::profiles::ProfilesState>()
        .active_profile()
    {
        Some(p) => p.notify,
        None => return,
    };
    if !passes_min_prio(&alert.prio, &cfg.min_prio) {
        return;
    }
    if !(cfg.dest_app || cfg.dest_chat || cfg.dest_discord || cfg.dest_tg) {
        return;
    }
    {
        let mut seen = match seen_store().lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if !dedup_should_send(&mut seen, &dedup_key(&alert), now_ms(), DEDUP_WINDOW_MS) {
            return;
        }
    }

    if cfg.dest_app {
        emit_app(app, &alert);
        crate::shell::native_notify(app, &alert.title, &alert.text);
    }
    if cfg.dest_chat {
        let app2 = app.clone();
        let text = format_game_chat(&alert);
        tauri::async_runtime::spawn(async move {
            let _ = crate::rustplus::send_chat(app2, text).await;
        });
    }
    if cfg.dest_discord {
        if let Some(wh) = clean(&cfg.discord_webhook) {
            let a = alert.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = send_discord(&wh, &a).await {
                    log::warn!("уведомление: {e}");
                }
            });
        }
    }
    if cfg.dest_tg {
        if let (Some(tok), Some(chat)) = (clean(&cfg.tg_token), clean(&cfg.tg_chat)) {
            let a = alert.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = send_tg(&tok, &chat, &a).await {
                    log::warn!("уведомление: {}", e.replace(tok.as_str(), "***"));
                }
            });
        }
    }
}

#[tauri::command]
pub async fn notify_test(app: AppHandle, target: String) -> Result<(), String> {
    let cfg: NotifyCfg = app
        .state::<crate::profiles::ProfilesState>()
        .active_profile()
        .map(|p| p.notify)
        .ok_or_else(|| "нет активной пачки".to_string())?;
    let alert = Alert {
        kind: "info".into(),
        prio: "low".into(),
        title: "hollow++".into(),
        text: "тест".into(),
    };
    let target = target.trim().to_lowercase();
    let all = target == "all";
    if !all && !matches!(target.as_str(), "discord" | "tg" | "app") {
        return Err("неизвестная цель уведомления (discord | tg | app | all)".into());
    }
    let mut errors: Vec<String> = Vec::new();

    if all || target == "app" {
        emit_app(&app, &alert);
    }
    if all || target == "discord" {
        match clean(&cfg.discord_webhook) {
            Some(wh) => {
                if let Err(e) = send_discord(&wh, &alert).await {
                    errors.push(e);
                }
            }
            None if !all => return Err("Discord: webhook не задан".into()),
            None => {}
        }
    }
    if all || target == "tg" {
        match (clean(&cfg.tg_token), clean(&cfg.tg_chat)) {
            (Some(tok), Some(chat)) => {
                if let Err(e) = send_tg(&tok, &chat, &alert).await {
                    errors.push(e);
                }
            }
            _ if !all => return Err("Telegram: не задан токен бота или chat id".into()),
            _ => {}
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn al(title: &str, text: &str) -> Alert {
        Alert { kind: "info".into(), prio: "low".into(), title: title.into(), text: text.into() }
    }

    #[test]
    fn prio_order() {
        assert!(prio_rank("low") < prio_rank("med"));
        assert!(prio_rank("med") < prio_rank("high"));
        assert_eq!(prio_rank(""), 0);
        assert_eq!(prio_rank("???"), 0);
        assert_eq!(prio_rank(" HIGH "), 2);
    }

    #[test]
    fn min_prio_filter() {
        assert!(passes_min_prio("low", ""));
        assert!(passes_min_prio("low", "low"));
        assert!(!passes_min_prio("low", "med"));
        assert!(passes_min_prio("med", "med"));
        assert!(!passes_min_prio("med", "high"));
        assert!(passes_min_prio("high", "high"));
    }

    #[test]
    fn truncate_basic() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("abcde", 5), "abcde");
        assert_eq!(truncate_chars("abcdef", 5), "abcd…");
        assert_eq!(truncate_chars("абвгде", 4), "абв…");
        assert_eq!(truncate_chars("abc", 0), "");
        assert_eq!(truncate_chars("abc", 1), "…");
    }

    #[test]
    fn limits_respected() {
        let long = "я".repeat(5000);
        assert_eq!(format_discord(&al("t", &long)).chars().count(), DISCORD_LIMIT);
        assert_eq!(format_telegram(&al("t", &long)).chars().count(), TELEGRAM_LIMIT);
        assert_eq!(format_game_chat(&al("t", &long)).chars().count(), GAME_CHAT_LIMIT);
        assert_eq!(format_discord(&al("t", "x")), "**t**\nx");
        assert_eq!(format_telegram(&al("", "x")), "x");
        assert_eq!(format_game_chat(&al("t", "a\nb")), "[t] a b");
    }

    #[test]
    fn dedup_window() {
        let mut m = HashMap::new();
        assert!(dedup_should_send(&mut m, "k", 1000, 30_000));
        assert!(!dedup_should_send(&mut m, "k", 20_000, 30_000));
        assert!(dedup_should_send(&mut m, "other", 20_000, 30_000));
        assert!(!dedup_should_send(&mut m, "k", 30_999, 30_000));
        assert!(dedup_should_send(&mut m, "k", 31_000, 30_000));
    }

    #[test]
    fn dedup_key_distinguishes() {
        assert_ne!(dedup_key(&al("a", "b")), dedup_key(&al("ab", "")));
        assert_eq!(dedup_key(&al("a", "b")), dedup_key(&al("a", "b")));
    }

    #[test]
    fn retry_after() {
        assert_eq!(parse_retry_after_ms("2"), Some(2000));
        assert_eq!(parse_retry_after_ms("0.5"), Some(500));
        assert_eq!(parse_retry_after_ms("999"), Some(MAX_RETRY_WAIT_MS));
        assert_eq!(parse_retry_after_ms("abc"), None);
        assert_eq!(parse_retry_after_ms("-1"), None);
    }
}
