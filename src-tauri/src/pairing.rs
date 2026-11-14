//! PAIRING: автоматическое получение данных Rust+ (как `fcm-register` + `fcm-listen` из liamcottle/rustplus.js).
//! Шаги: FCM-регистрация -> Expo token -> логин Steam в отдельном окне -> регистрация в Rust+ companion API ->
//! приём push по MCS -> разбор уведомлений о сервере/устройствах.
//! Секреты хранятся только в `app_data_dir()/pairing.json`, наружу (события, логи) не отдаются.
mod fcm;
mod mcs;
mod notif;
mod store;

use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};

use store::PairingStore;

const LOGIN_URL: &str = "https://companion-rust.facepunch.com/login";
const LOGIN_LABEL: &str = "pair_login";
/// Псевдо-адрес, на который JS-мост отправляет токен; навигация перехватывается в `on_navigation` и отменяется.
const CALLBACK_HOST: &str = "hollow-pair.invalid";
const LOGIN_TIMEOUT_SECS: u64 = 600;

/// Сайт логина Rust+ после входа через Steam вызывает `window.ReactNativeWebView.postMessage(JSON{SteamId,Token})`
/// (так сделано в rustplus.js cli/pair.html). Мы подставляем такой объект на каждой странице, а полученный Token
/// передаём в Rust через навигацию на CALLBACK_HOST (вместо локального HTTP-сервера на localhost:3000).
const LOGIN_SCRIPT: &str = r#"(function(){try{
var install=function(){
  if(window.ReactNativeWebView&&window.ReactNativeWebView.__hollow){return;}
  window.ReactNativeWebView={__hollow:true,postMessage:function(m){
    try{var a=(typeof m==='string')?JSON.parse(m):m;
      if(a&&a.Token){window.location.href='https://hollow-pair.invalid/callback?token='+encodeURIComponent(a.Token);}
    }catch(e){}
  }};
};
install();setInterval(install,250);
}catch(e){}})();"#;

static TASK: Mutex<Option<(tauri::async_runtime::JoinHandle<()>, Arc<AtomicBool>)>> = Mutex::new(None);

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn emit_state(app: &AppHandle, state: &str, msg: &str) {
    let _ = app.emit("pairing", json!({ "state": state, "msg": msg }));
}

fn data_dir(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    app.path().app_data_dir().map_err(|e| format!("нет папки данных: {e}"))
}

/// Что ещё нужно сделать до начала прослушивания.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub fcm: bool,
    pub expo: bool,
    pub login: bool,
    pub register: bool,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        !(self.fcm || self.expo || self.login || self.register)
    }
}

pub fn plan(st: &PairingStore, now: u64) -> Plan {
    let fcm = st.fcm.is_none();
    let expo = fcm || st.expo_token.is_none();
    let login = st.auth_token.is_none() || (st.registered_at.is_some() && st.auth_stale(now));
    let register = login || expo || st.registered_at.is_none();
    Plan { fcm, expo, login, register }
}

/// Токен из URL-колбэка JS-моста (`https://hollow-pair.invalid/callback?token=...`).
pub fn extract_token(url: &tauri::Url) -> Option<String> {
    if url.host_str() != Some(CALLBACK_HOST) {
        return None;
    }
    url.query_pairs().find(|(k, _)| k == "token").map(|(_, v)| v.into_owned()).filter(|t| !t.is_empty())
}

fn close_login_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(LOGIN_LABEL) {
        let _ = w.destroy();
    }
}

/// Открывает окно логина Steam (только по pair_start) и ждёт токен Rust+.
async fn steam_login(app: &AppHandle) -> Result<String, String> {
    close_login_window(app);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Option<String>>();
    let tx_nav = tx.clone();
    let url: tauri::Url = LOGIN_URL.parse().map_err(|_| "некорректный URL логина".to_string())?;
    let win = WebviewWindowBuilder::new(app, LOGIN_LABEL, WebviewUrl::External(url))
        .title("Rust+ - вход через Steam")
        .inner_size(520.0, 760.0)
        .center()
        .initialization_script(LOGIN_SCRIPT)
        .on_navigation(move |u| {
            if let Some(t) = extract_token(u) {
                let _ = tx_nav.send(Some(t));
                return false;
            }
            u.host_str() != Some(CALLBACK_HOST)
        })
        .build()
        .map_err(|e| format!("не удалось открыть окно входа: {e}"))?;
    let tx_close = tx.clone();
    win.on_window_event(move |e| {
        if matches!(e, WindowEvent::CloseRequested { .. } | WindowEvent::Destroyed) {
            let _ = tx_close.send(None);
        }
    });
    drop(tx);
    let res = tokio::time::timeout(std::time::Duration::from_secs(LOGIN_TIMEOUT_SECS), rx.recv()).await;
    close_login_window(app);
    match res {
        Ok(Some(Some(t))) => Ok(t),
        Ok(Some(None)) | Ok(None) => Err("окно входа закрыто до завершения логина".into()),
        Err(_) => Err("время ожидания входа через Steam истекло".into()),
    }
}

fn save(dir: &std::path::Path, st: &PairingStore) -> Result<(), String> {
    store::save(dir, st)
}

/// Регистрация (по необходимости) и затем бесконечное прослушивание.
async fn run(app: AppHandle, interactive: bool) -> Result<(), String> {
    let dir = data_dir(&app)?;
    let mut st = store::load(&dir);
    if interactive {
        let p = plan(&st, now_secs());
        if !p.is_empty() {
            emit_state(&app, "registering", "Регистрация устройства в FCM...");
            if p.fcm {
                st.fcm = Some(fcm::fcm_register().await?);
                st.expo_token = None;
                save(&dir, &st)?;
            }
            if p.expo {
                let tok = st.fcm.as_ref().map(|f| f.fcm_token.clone()).unwrap_or_default();
                emit_state(&app, "registering", "Получение Expo push token...");
                st.expo_token = Some(fcm::expo_token(&tok).await?);
                st.registered_at = None;
                save(&dir, &st)?;
            }
            if p.login {
                emit_state(&app, "login", "Войдите через Steam в открывшемся окне");
                st.auth_token = Some(steam_login(&app).await?);
                st.registered_at = None;
                save(&dir, &st)?;
            }
            if p.register {
                emit_state(&app, "registering", "Регистрация в Rust+...");
                let auth = st.auth_token.clone().unwrap_or_default();
                let expo = st.expo_token.clone().unwrap_or_default();
                fcm::rustplus_register(&auth, &expo).await?;
                st.registered_at = Some(now_secs());
                save(&dir, &st)?;
            }
        }
    }
    if !st.is_ready() {
        return Err("регистрация Rust+ не завершена: нажмите «Подключить через Steam»".into());
    }
    listen(app, dir, st).await
}

async fn listen(app: AppHandle, dir: std::path::PathBuf, st: PairingStore) -> Result<(), String> {
    let creds = st.fcm.clone().ok_or("нет FCM-данных")?;
    let shared = Arc::new(Mutex::new(st));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<mcs::McsEvent>();
    {
        let app = app.clone();
        let shared = shared.clone();
        let dir = dir.clone();
        tauri::async_runtime::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if let mcs::McsEvent::Data { persistent_id, app_data, encrypted } = ev {
                    handle_data(&app, &shared, &dir, persistent_id, app_data, encrypted).await;
                }
            }
        });
    }
    let mut retry: u32 = 0;
    loop {
        emit_state(&app, "waiting", "Ожидаю Pair в игре: ESC, Rust+, Pair with Server");
        let pids = shared.lock().map(|s| s.persistent_ids.clone()).unwrap_or_default();
        let logged_in = Arc::new(AtomicBool::new(false));
        let li = logged_in.clone();
        let tx2 = tx.clone();
        let res = mcs::session(&creds.android_id, &creds.security_token, &pids, move |ev| {
            if matches!(ev, mcs::McsEvent::LoggedIn) {
                li.store(true, Ordering::SeqCst);
            }
            let _ = tx2.send(ev);
        })
        .await;
        if logged_in.load(Ordering::SeqCst) {
            retry = 0;
        }
        retry = (retry + 1).min(15);
        let msg = res.err().unwrap_or_default();
        log::warn!("pairing: MCS-сессия завершена: {}", fcm::scrub(&msg));
        emit_state(&app, "waiting", &format!("Связь с Google потеряна, повтор через {retry} с"));
        tokio::time::sleep(std::time::Duration::from_secs(retry as u64)).await;
    }
}

async fn handle_data(
    app: &AppHandle,
    shared: &Arc<Mutex<PairingStore>>,
    dir: &std::path::Path,
    persistent_id: Option<String>,
    app_data: Vec<(String, String)>,
    encrypted: bool,
) {
    if let Some(pid) = persistent_id.as_deref() {
        let fresh = {
            let mut g = match shared.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            let f = g.remember(pid);
            if f {
                let _ = store::save(dir, &g);
            }
            f
        };
        if !fresh {
            return;
        }
    }
    if encrypted {
        // В референсе (push-receiver) расшифровка шифрованных сообщений сломана (`keys` не определён),
        // а Rust+ шлёт без crypto-key; у нас расшифровка не реализована.
        emit_state(app, "waiting", "Получено зашифрованное уведомление: расшифровка не поддерживается");
        return;
    }
    match notif::parse(&app_data) {
        Ok(notif::Notif::Server(sp)) => apply_server(app, sp).await,
        Ok(notif::Notif::Entity(ep)) => apply_entity(app, ep),
        Ok(notif::Notif::Ignored(_)) => {}
        Err(e) => emit_state(app, "error", &format!("Не удалось разобрать уведомление Rust+: {e}")),
    }
}

async fn apply_server(app: &AppHandle, sp: notif::ServerPair) {
    let creds = crate::types::ServerCreds {
        ip: sp.ip.clone(),
        port: sp.port,
        player_id: sp.player_id.clone(),
        player_token: sp.player_token.clone(),
    };
    let profiles = app.state::<crate::profiles::ProfilesState>();
    if let Err(e) = profiles.set_active_server(creds.clone()) {
        emit_state(app, "error", &format!("Сервер получен, но не сохранён: {e}"));
        return;
    }
    let _ = app.emit("pair_server", json!({ "name": sp.name, "ip": sp.ip, "port": sp.port }));
    emit_state(app, "paired", &format!("Сервер «{}» сопряжён", sp.name));
    if let Err(e) = crate::rustplus::connect(app.clone(), creds).await {
        emit_state(app, "error", &format!("Сервер сохранён, подключение не удалось: {}", fcm::scrub(&e)));
    }
}

fn apply_entity(app: &AppHandle, ep: notif::EntityPair) {
    let profiles = app.state::<crate::profiles::ProfilesState>();
    if let (Some(active), Some(ip), Some(port)) = (profiles.active_server(), ep.ip.as_ref(), ep.port) {
        if active.ip != *ip || active.port != port {
            emit_state(app, "waiting", "Устройство с другого сервера пропущено (не совпадает с активной пачкой)");
            return;
        }
    }
    let dev = crate::types::PairedDevice { id: ep.id.clone(), name: ep.name.clone(), kind: ep.kind.clone() };
    match profiles.upsert_active_device(dev) {
        Ok(()) => {
            let _ = app.emit("pair_device", json!({ "id": ep.id, "name": ep.name, "kind": ep.kind }));
        }
        Err(e) => emit_state(app, "error", &format!("Устройство получено, но не сохранено: {e}")),
    }
}

fn abort_task() {
    if let Ok(mut g) = TASK.lock() {
        if let Some((h, _)) = g.take() {
            h.abort();
        }
    }
}

fn start(app: &AppHandle, interactive: bool) {
    let done = Arc::new(AtomicBool::new(false));
    let d2 = done.clone();
    let a = app.clone();
    let h = tauri::async_runtime::spawn(async move {
        if let Err(e) = run(a.clone(), interactive).await {
            emit_state(&a, "error", &fcm::scrub(&e));
        }
        d2.store(true, Ordering::SeqCst);
    });
    if let Ok(mut g) = TASK.lock() {
        *g = Some((h, done));
    }
}

fn running() -> bool {
    TASK.lock().ok().and_then(|g| g.as_ref().map(|(_, d)| !d.load(Ordering::SeqCst))).unwrap_or(false)
}

/// Автозапуск слушателя при старте приложения, если регистрация уже сохранена. Окон не открывает.
pub async fn resume(app: &AppHandle) {
    if running() {
        return;
    }
    let Ok(dir) = data_dir(app) else { return };
    if store::load(&dir).is_ready() {
        start(app, false);
    }
}

#[tauri::command]
pub async fn pair_start(app: AppHandle) -> Result<(), String> {
    let dir = data_dir(&app)?;
    let st = store::load(&dir);
    let needs_work = !plan(&st, now_secs()).is_empty();
    if running() && !needs_work {
        emit_state(&app, "waiting", "Ожидаю Pair в игре: ESC, Rust+, Pair with Server");
        return Ok(());
    }
    abort_task();
    close_login_window(&app);
    start(&app, true);
    Ok(())
}

#[tauri::command]
pub async fn pair_cancel(app: AppHandle) -> Result<(), String> {
    abort_task();
    close_login_window(&app);
    emit_state(&app, "idle", "");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fcm::FcmCreds;

    fn full() -> PairingStore {
        PairingStore {
            fcm: Some(FcmCreds { android_id: "1".into(), security_token: "2".into(), fcm_token: "t".into() }),
            expo_token: Some("e".into()),
            auth_token: Some("a".into()),
            registered_at: Some(1000),
            persistent_ids: vec![],
        }
    }

    #[test]
    fn plan_empty_store_needs_everything() {
        let p = plan(&PairingStore::default(), 5);
        assert_eq!(p, Plan { fcm: true, expo: true, login: true, register: true });
    }

    #[test]
    fn plan_ready_store_is_empty_until_stale() {
        let st = full();
        assert!(plan(&st, 2000).is_empty());
        let p = plan(&st, 1001 + store::AUTH_MAX_AGE_SECS);
        assert_eq!(p, Plan { fcm: false, expo: false, login: true, register: true });
    }

    #[test]
    fn plan_register_retry_without_login() {
        let mut st = full();
        st.registered_at = None;
        assert_eq!(plan(&st, 2000), Plan { fcm: false, expo: false, login: false, register: true });
        let mut st = full();
        st.expo_token = None;
        let p = plan(&st, 2000);
        assert!(p.expo && p.register && !p.login);
        let mut st = full();
        st.fcm = None;
        let p = plan(&st, 2000);
        assert!(p.fcm && p.expo && p.register);
    }

    #[test]
    fn callback_token_extraction() {
        let u: tauri::Url = "https://hollow-pair.invalid/callback?token=abc%2Bdef".parse().unwrap();
        assert_eq!(extract_token(&u).as_deref(), Some("abc+def"));
        let u: tauri::Url = "https://hollow-pair.invalid/callback".parse().unwrap();
        assert_eq!(extract_token(&u), None);
        let u: tauri::Url = "https://evil.example/callback?token=x".parse().unwrap();
        assert_eq!(extract_token(&u), None);
        let u: tauri::Url = "https://hollow-pair.invalid/callback?token=".parse().unwrap();
        assert_eq!(extract_token(&u), None);
    }

    #[test]
    fn login_script_targets_callback() {
        assert!(LOGIN_SCRIPT.contains(CALLBACK_HOST));
        assert!(LOGIN_SCRIPT.contains("ReactNativeWebView"));
        assert!(LOGIN_SCRIPT.contains("a.Token"));
    }
}
