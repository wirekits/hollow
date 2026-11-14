//! Хранилище `app_data_dir()/pairing.json` (атомарная запись: tmp + rename).
//! Содержит секреты (FCM/Expo/Rust+ auth). Шифрование DPAPI НЕ делается: файл лежит в профиле пользователя.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::fcm::FcmCreds;

const MAX_PERSISTENT_IDS: usize = 100;
/// Токен Rust+ живёт около двух недель (docs/PairingFlow.md rustplus.js); перелогиниваемся с запасом.
pub const AUTH_MAX_AGE_SECS: u64 = 13 * 24 * 3600;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PairingStore {
    pub fcm: Option<FcmCreds>,
    pub expo_token: Option<String>,
    pub auth_token: Option<String>,
    /// unix-секунды момента успешной регистрации в Rust+ companion API.
    pub registered_at: Option<u64>,
    /// последние обработанные persistentId (чтобы не обрабатывать повторно после перезапуска).
    pub persistent_ids: Vec<String>,
}

pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("pairing.json")
}

pub fn load(dir: &Path) -> PairingStore {
    std::fs::read(path_in(dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(dir: &Path, st: &PairingStore) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("не удалось создать папку данных: {e}"))?;
    let data = serde_json::to_vec_pretty(st).map_err(|e| e.to_string())?;
    let p = path_in(dir);
    let tmp = dir.join("pairing.json.tmp");
    std::fs::write(&tmp, data).map_err(|e| format!("запись pairing.json: {e}"))?;
    std::fs::rename(&tmp, &p).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("запись pairing.json: {e}")
    })
}

impl PairingStore {
    /// Есть всё, чтобы слушать push (FCM + Expo + регистрация в Rust+).
    pub fn is_ready(&self) -> bool {
        self.fcm.is_some() && self.expo_token.is_some() && self.auth_token.is_some() && self.registered_at.is_some()
    }

    /// Токен Rust+ устарел (или времени регистрации нет) и нужен новый логин Steam.
    pub fn auth_stale(&self, now: u64) -> bool {
        match (self.auth_token.as_ref(), self.registered_at) {
            (Some(_), Some(t)) => now.saturating_sub(t) > AUTH_MAX_AGE_SECS,
            _ => true,
        }
    }

    /// Запоминает persistentId; true, если он новый (дубликаты не обрабатываются).
    pub fn remember(&mut self, pid: &str) -> bool {
        if self.persistent_ids.iter().any(|p| p == pid) {
            return false;
        }
        self.persistent_ids.push(pid.to_string());
        if self.persistent_ids.len() > MAX_PERSISTENT_IDS {
            let extra = self.persistent_ids.len() - MAX_PERSISTENT_IDS;
            self.persistent_ids.drain(..extra);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(n: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hollow_pairing_test_{n}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn roundtrip_and_atomic() {
        let d = tmpdir("rt");
        assert!(!load(&d).is_ready());
        let st = PairingStore {
            fcm: Some(FcmCreds { android_id: "1".into(), security_token: "2".into(), fcm_token: "t".into() }),
            expo_token: Some("e".into()),
            auth_token: Some("a".into()),
            registered_at: Some(100),
            persistent_ids: vec!["p".into()],
        };
        save(&d, &st).unwrap();
        assert!(!d.join("pairing.json.tmp").exists());
        let l = load(&d);
        assert!(l.is_ready());
        assert_eq!(l.fcm.unwrap().android_id, "1");
        assert_eq!(l.persistent_ids, vec!["p".to_string()]);
        std::fs::write(path_in(&d), b"{broken").unwrap();
        assert!(!load(&d).is_ready());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn stale_logic() {
        let mut st = PairingStore::default();
        assert!(st.auth_stale(1000));
        st.auth_token = Some("a".into());
        st.registered_at = Some(1000);
        assert!(!st.auth_stale(1000 + AUTH_MAX_AGE_SECS));
        assert!(st.auth_stale(1001 + AUTH_MAX_AGE_SECS));
    }

    #[test]
    fn remember_dedup_and_cap() {
        let mut st = PairingStore::default();
        assert!(st.remember("a"));
        assert!(!st.remember("a"));
        for i in 0..150 {
            st.remember(&format!("p{i}"));
        }
        assert_eq!(st.persistent_ids.len(), MAX_PERSISTENT_IDS);
        assert_eq!(st.persistent_ids.last().unwrap(), "p149");
        assert!(st.remember("a"));
    }
}
