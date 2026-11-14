mod a2s;
mod automation;
mod battlemetrics;
mod notify;
mod overlay;
mod pairing;
mod profiles;
mod rustplus;
mod shell;
mod steam;
mod types;
mod watcher;

use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // в дереве два бэкенда rustls (ring у нас, aws-lc у steam-vent): без явного выбора rustls паникует
    let _ = rustls::crypto::ring::default_provider().install_default();
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| shell::raise_main(app)))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .manage(profiles::ProfilesState::default())
        .manage(rustplus::RpState::default())
        .manage(battlemetrics::BmState::default())
        .manage(automation::AutomationState::default())
        .setup(|app| {
            shell::setup(app)?;
            let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
            let st = app.state::<profiles::ProfilesState>();
            st.load(&dir);
            let server = st.active_server();
            let token = st.active_bm_token();
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                battlemetrics::configure(&handle, token).await;
                if let Some(creds) = server {
                    let _ = rustplus::connect(handle.clone(), creds).await;
                }
                automation::start(&handle).await;
                pairing::resume(&handle).await;
                watcher::start(&handle);
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            profiles::profiles_list,
            profiles::profile_create,
            profiles::profile_update,
            profiles::profile_delete,
            profiles::profile_switch,
            rustplus::rp_connect,
            rustplus::rp_disconnect,
            rustplus::set_device,
            rustplus::send_chat,
            battlemetrics::set_bm_token,
            battlemetrics::lookup_player,
            battlemetrics::track_add,
            battlemetrics::track_remove,
            notify::notify_test,
            automation::group_set,
            overlay::overlay_toggle,
            pairing::pair_start,
            pairing::pair_cancel,
            shell::app_settings_get,
            shell::app_settings_set,
            watcher::watcher_login,
            watcher::watcher_code,
            watcher::watcher_logout,
            watcher::watcher_status,
            watcher::watcher_add_friend,
        ])
        .run(tauri::generate_context!())
        .expect("error while running hollow++");
}
