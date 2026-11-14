//! Сырая статистика Rust (GetUserStatsForGame, appid 252490) и производные показатели для страницы профиля.
//!
//! ИМЕНА КЛЮЧЕЙ: сверены с РЕАЛЬНОЙ схемой (ISteamUserStats/GetSchemaForGame, 151 имя, живой запрос).
//! Чего нет в схеме, в таблицах нет. В схеме НЕТ зон попаданий (head/chest/legs): есть только `headshot` и
//! `headshots`, поэтому `hitZones` всегда null. Также нет acquired_sulfur/coal/hqm: сера только из `harvest.sulfur_ore`.
//! Имена нормализуются: нижний регистр, всё не-буквенно-цифровое -> `_` (`harvest.wood` == `harvest_wood`,
//! `acquired_metal.ore` == `acquired_metal_ore`). Дубли `harvest.*` и `harvested_*` берутся по максимуму.
//!
//! ДОПУЩЕНИЕ про `headshot` и `headshots` (оба есть в схеме): по имени и по тому, что rankeval.gg показывает `headshot`
//! как "Headshots" и считает от него долю хедшотов, приоритет у `headshot` (попадания в голову по игрокам);
//! `headshots` (стоит в схеме рядом со счётчиками выстрелов, возможно, общий/старый счётчик) используется
//! только как запасной вариант. Это НЕ подтверждено; проверить на живом аккаунте сравнением значений.
//!
//! Радар, playerType - НАША ОЦЕНКА, не данные Steam. Формула: sat(x, k) = x / (x + k) * 100 (насыщение:
//! при x == k даёт 50, никогда не достигает 100). Оси со смешанными компонентами - взвешенное среднее
//! только по найденным компонентам. Если не найдено ни одного компонента, ось = null.
//!
//! ТОЧНОСТЬ: по классу = попадания по игрокам / выстрелы (bullet: bullet_hit_player/bullet_fired,
//! arrow: arrow_hit_player/arrow_fired, shotgun: shotgun_hit_player/shotgun_fired). Класс с hit > fired
//! (у дробовика попадания могут считаться по дробинам) или fired == 0 даёт null и в общую сумму не входит.
//! Общая accuracyPct = сумма попаданий / сумма выстрелов по классам, у которых оба числа найдены и корректны.
//! shotsFired/shotsHit в derived - те же суммы.
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub const MAX_STATS: usize = 500;
pub const MAX_RESOURCES: usize = 8;

// ---- константы радара (половинные точки насыщения: значение, при котором ось = 50) ----
const K_ACC_PCT: f64 = 20.0; // точность в %
const K_HS_PCT: f64 = 15.0; // доля хедшотов в %
const W_ACC: f64 = 0.6;
const W_HS: f64 = 0.4;
const K_PVP_KILLS: f64 = 500.0;
const K_PVP_KDR: f64 = 1.5;
const K_PVE_KILLS: f64 = 300.0; // ученые + животные
const K_GATHER_TOTAL: f64 = 300_000.0; // сумма добытых ресурсов
const K_BUILD_BLOCKS: f64 = 3_000.0; // поставлено + улучшено блоков
const K_SURV_COMFORT_H: f64 = 50.0; // часы в комфорте (comfort_duration в секундах)
const K_SURV_CALORIES: f64 = 100_000.0;
const K_SURV_WATER: f64 = 50_000.0;
// ---- playerType ----
const MIN_TYPE_SCORE: f64 = 20.0; // ниже этого ни одна ось не считается профилем
const BALANCE_MARGIN: f64 = 8.0; // если лидер опережает второго меньше чем на это, тип balanced

// ---- таблицы ключей (нормализованные, все есть в реальной схеме) ----
const KILLS_KEYS: &[&str] = &["kill_player"];
const DEATHS_KEYS: &[&str] = &["deaths"];
/// Приоритет: headshot, затем headshots (см. допущение выше).
const HEADSHOT_KEYS: &[&str] = &["headshot", "headshots"];
const PVE_KILL_KEYS: &[&str] = &[
    "kill_scientist", "kill_bear", "kill_boar", "kill_stag", "kill_chicken", "kill_horse", "kill_wolf",
];
const BLOCKS_PLACED_KEYS: &[&str] = &["placed_blocks"];
const BLOCKS_UPGRADED_KEYS: &[&str] = &["upgraded_blocks"];
const COMFORT_KEYS: &[&str] = &["comfort_duration"];
const CALORIES_KEYS: &[&str] = &["calories_consumed"];
const WATER_KEYS: &[&str] = &["water_consumed"];
/// (класс, ключ выстрелов, ключ попаданий по игрокам)
const WEAPON_CLASSES: &[(&str, &str, &str)] = &[
    ("bullet", "bullet_fired", "bullet_hit_player"),
    ("arrow", "arrow_fired", "arrow_hit_player"),
    ("shotgun", "shotgun_fired", "shotgun_hit_player"),
];
/// Префиксы ключей ресурсов (harvest.* / harvested_* / acquired_*).
const RES_PREFIXES: &[&str] = &["harvest_", "harvested_", "acquired_"];
/// (канонический ключ, суффиксы после префикса; только реально существующие в схеме)
const RES_TABLE: &[(&str, &[&str])] = &[
    ("wood", &["wood"]),
    ("stone", &["stones"]),
    ("metal", &["metal_ore"]),
    ("sulfur", &["sulfur_ore"]),
    ("cloth", &["cloth"]),
    ("leather", &["leather"]),
    ("fat", &["fat_animal"]),
    ("bone", &["bone_fragments"]),
    ("scrap", &["scrap"]),
    ("oil", &["lowgradefuel"]),
];

pub fn norm(k: &str) -> String {
    k.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect()
}

/// Все числовые статистики {имя: значение}, не более MAX_STATS, по алфавиту.
pub fn raw_stats(v: &Value) -> Option<BTreeMap<String, f64>> {
    let arr = v.pointer("/playerstats/stats")?.as_array()?;
    let mut m = BTreeMap::new();
    for s in arr {
        if let (Some(n), Some(val)) = (s.get("name").and_then(|x| x.as_str()), s.get("value").and_then(|x| x.as_f64())) {
            if val.is_finite() {
                m.insert(n.to_string(), val);
            }
        }
    }
    while m.len() > MAX_STATS {
        m.pop_last();
    }
    Some(m)
}

fn num(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() < 9.0e15 {
        json!(f as i64)
    } else {
        json!(f)
    }
}

pub fn stats_json(raw: &BTreeMap<String, f64>) -> Value {
    let mut o = Map::new();
    for (k, v) in raw {
        o.insert(k.clone(), num(*v));
    }
    Value::Object(o)
}

fn normalized(raw: &BTreeMap<String, f64>) -> BTreeMap<String, f64> {
    let mut m: BTreeMap<String, f64> = BTreeMap::new();
    for (k, v) in raw {
        let e = m.entry(norm(k)).or_insert(*v);
        if *v > *e {
            *e = *v;
        }
    }
    m
}

fn get(m: &BTreeMap<String, f64>, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| m.get(*k).copied())
}

/// Сумма найденных ключей; None, если не найден ни один.
fn sum_found(m: &BTreeMap<String, f64>, keys: &[&str]) -> Option<f64> {
    let found: Vec<f64> = keys.iter().filter_map(|k| m.get(*k).copied()).collect();
    if found.is_empty() {
        None
    } else {
        Some(found.iter().sum())
    }
}

/// Насыщающая нормализация 0..100.
pub fn sat(x: f64, k: f64) -> f64 {
    if x <= 0.0 || k <= 0.0 {
        0.0
    } else {
        x / (x + k) * 100.0
    }
}

/// Взвешенное среднее найденных компонентов (значение 0..100, вес). None, если компонентов нет.
fn wavg(parts: &[Option<(f64, f64)>]) -> Option<f64> {
    let (mut s, mut w) = (0.0, 0.0);
    for (v, wt) in parts.iter().flatten() {
        s += v * wt;
        w += wt;
    }
    if w > 0.0 {
        Some(s / w)
    } else {
        None
    }
}

fn r0(x: f64) -> i64 {
    x.round().clamp(0.0, 100.0) as i64
}
fn r1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Ресурсы: до MAX_RESOURCES крупнейших, [(ключ, количество)]. Дубли берутся по максимуму.
pub fn resources(raw: &BTreeMap<String, f64>) -> Vec<(String, f64)> {
    let m = normalized(raw);
    let mut best: BTreeMap<&'static str, f64> = BTreeMap::new();
    for (k, v) in &m {
        if *v <= 0.0 {
            continue;
        }
        for p in RES_PREFIXES {
            let Some(rest) = k.strip_prefix(p) else { continue };
            if let Some((canon, _)) = RES_TABLE.iter().find(|(_, sfx)| sfx.contains(&rest)) {
                let e = best.entry(canon).or_insert(*v);
                if *v > *e {
                    *e = *v;
                }
            }
        }
    }
    let mut out: Vec<(String, f64)> = best.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    out.truncate(MAX_RESOURCES);
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct WeaponClass {
    pub name: &'static str,
    pub fired: Option<f64>,
    pub hit: Option<f64>,
    /// % попаданий по игрокам; None, если данных нет или они противоречивы
    pub acc: Option<f64>,
}

pub fn weapon_classes(raw: &BTreeMap<String, f64>) -> Vec<WeaponClass> {
    let m = normalized(raw);
    WEAPON_CLASSES
        .iter()
        .map(|(name, fk, hk)| {
            let fired = m.get(*fk).copied();
            let hit = m.get(*hk).copied();
            let acc = match (fired, hit) {
                (Some(f), Some(h)) if f > 0.0 && h >= 0.0 && h <= f => Some(r1(h / f * 100.0)),
                _ => None,
            };
            WeaponClass { name, fired, hit, acc }
        })
        .collect()
}

/// (сумма выстрелов, сумма попаданий, общая точность %) по классам с корректной точностью.
pub fn total_accuracy(ws: &[WeaponClass]) -> Option<(f64, f64, f64)> {
    let (mut f, mut h) = (0.0, 0.0);
    for w in ws.iter().filter(|w| w.acc.is_some()) {
        f += w.fired.unwrap_or(0.0);
        h += w.hit.unwrap_or(0.0);
    }
    if f > 0.0 {
        Some((f, h, r1(h / f * 100.0)))
    } else {
        None
    }
}

/// Оси радара 0..100 (None, если данных нет).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Radar {
    pub aim: Option<f64>,
    pub pvp: Option<f64>,
    pub pve: Option<f64>,
    pub gathering: Option<f64>,
    pub building: Option<f64>,
    pub survival: Option<f64>,
}

/// Тип игрока по осям (aim не участвует: это качество, а не стиль). Оценка, не данные Steam.
/// Лидер должен набрать >= MIN_TYPE_SCORE, иначе unknown; отрыв от второго < BALANCE_MARGIN -> balanced.
pub fn player_type(r: &Radar) -> &'static str {
    let mut axes: Vec<(&'static str, f64)> = Vec::new();
    for (name, v) in [
        ("pvp_roamer", r.pvp),
        ("pve_hunter", r.pve),
        ("farmer", r.gathering),
        ("builder", r.building),
        ("survivor", r.survival),
    ] {
        if let Some(x) = v {
            axes.push((name, x));
        }
    }
    if axes.is_empty() {
        return "unknown";
    }
    axes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    if axes[0].1 < MIN_TYPE_SCORE {
        return "unknown";
    }
    if axes.len() > 1 && axes[0].1 - axes[1].1 < BALANCE_MARGIN {
        return "balanced";
    }
    axes[0].0
}

/// Силуэт "body" (только пули): headPct/bodyPct считаются от попаданий по игрокам, missPct от выстрелов.
/// ДОПУЩЕНИЕ (не подтверждено): `headshot` = попадания в голову по игрокам. Если на реальном аккаунте
/// headshot > bullet_hit_player, процент зажимается до 100 (headPct) / 0 (bodyPct). hit > fired даёт missPct = 0.
/// null целиком, если нет bullet_fired или bullet_hit_player, либо fired == 0; при hit == 0 или без headshot
/// head/body = null.
pub fn body(m: &BTreeMap<String, f64>, headshots: Option<f64>) -> Value {
    let (Some(fired), Some(hit)) = (m.get("bullet_fired").copied(), m.get("bullet_hit_player").copied()) else {
        return Value::Null;
    };
    if fired <= 0.0 || hit < 0.0 {
        return Value::Null;
    }
    let miss = r1(((fired - hit) / fired * 100.0).clamp(0.0, 100.0));
    let (head, bod) = match headshots {
        Some(h) if hit > 0.0 && h >= 0.0 => (
            Some(r1((h / hit * 100.0).clamp(0.0, 100.0))),
            Some(r1(((hit - h) / hit * 100.0).clamp(0.0, 100.0))),
        ),
        _ => (None, None),
    };
    json!({
        "headPct": head, "bodyPct": bod, "missPct": miss,
        "fired": num(fired), "hit": num(hit), "headshots": headshots.map(num),
    })
}

/// Итоговый объект "derived". Любой отсутствующий показатель = null; hitZones всегда null (нет в схеме Rust).
pub fn derive(raw: &BTreeMap<String, f64>) -> Value {
    let m = normalized(raw);
    let kills = get(&m, KILLS_KEYS);
    let deaths = get(&m, DEATHS_KEYS);
    let hs = get(&m, HEADSHOT_KEYS);

    let kdr_v = match (kills, deaths) {
        (Some(k), Some(d)) => Some(super::kdr(k as i64, d as i64)),
        _ => None,
    };
    let ws = weapon_classes(raw);
    let total = total_accuracy(&ws);
    let acc = total.map(|t| t.2);
    // доля хедшотов от попаданий по игрокам; не считаем, если данные противоречивы
    let hs_pct = match (hs, total) {
        (Some(s), Some((_, h, _))) if h > 0.0 && s >= 0.0 && s <= h => Some(r1(s / h * 100.0)),
        _ => None,
    };

    let res = resources(raw);
    let res_total: f64 = res.iter().map(|r| r.1).sum();
    let blocks = match (sum_found(&m, BLOCKS_PLACED_KEYS), sum_found(&m, BLOCKS_UPGRADED_KEYS)) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    };

    let radar = Radar {
        aim: wavg(&[acc.map(|a| (sat(a, K_ACC_PCT), W_ACC)), hs_pct.map(|h| (sat(h, K_HS_PCT), W_HS))]),
        pvp: wavg(&[
            kills.map(|k| (sat(k, K_PVP_KILLS), 0.5)),
            kdr_v.map(|d| (sat(d, K_PVP_KDR), 0.5)),
        ]),
        pve: sum_found(&m, PVE_KILL_KEYS).map(|x| sat(x, K_PVE_KILLS)),
        gathering: if res.is_empty() { None } else { Some(sat(res_total, K_GATHER_TOTAL)) },
        building: blocks.map(|b| sat(b, K_BUILD_BLOCKS)),
        survival: wavg(&[
            get(&m, COMFORT_KEYS).map(|s| (sat(s / 3600.0, K_SURV_COMFORT_H), 1.0)),
            get(&m, CALORIES_KEYS).map(|c| (sat(c, K_SURV_CALORIES), 1.0)),
            get(&m, WATER_KEYS).map(|w| (sat(w, K_SURV_WATER), 1.0)),
        ]),
    };
    let ptype = player_type(&radar);
    let ri = |x: Option<f64>| x.map(r0);

    let mut weapons = Map::new();
    for w in &ws {
        weapons.insert(
            w.name.to_string(),
            json!({"fired": w.fired.map(num), "hit": w.hit.map(num), "accuracyPct": w.acc}),
        );
    }

    json!({
        "kills": kills.map(num),
        "deaths": deaths.map(num),
        "kdr": kdr_v,
        "headshots": hs.map(num),
        "shotsFired": total.map(|t| num(t.0)),
        "shotsHit": total.map(|t| num(t.1)),
        "accuracyPct": acc,
        "headshotPct": hs_pct,
        "weapons": Value::Object(weapons),
        "body": body(&m, hs),
        "radar": {
            "aim": ri(radar.aim), "pvp": ri(radar.pvp), "pve": ri(radar.pve),
            "gathering": ri(radar.gathering), "building": ri(radar.building), "survival": ri(radar.survival),
        },
        "playerType": ptype,
        "resources": res.iter().map(|(k, v)| json!({"key": k, "amount": num(*v)})).collect::<Vec<_>>(),
        "hitZones": Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Value {
        json!({"playerstats":{"steamID":"1","gameName":"Rust","stats":[
            {"name":"kill_player","value":67},{"name":"deaths","value":121},
            {"name":"bullet_fired","value":6000},{"name":"bullet_hit_player","value":900},
            {"name":"arrow_fired","value":400},{"name":"arrow_hit_player","value":100},
            {"name":"shotgun_fired","value":100},{"name":"shotgun_hit_player","value":0},
            {"name":"headshot","value":150},{"name":"headshots","value":40},
            {"name":"harvest.wood","value":90397},{"name":"harvested_wood","value":80000},
            {"name":"harvest.stones","value":50000},{"name":"harvested_stones","value":51000},
            {"name":"harvest.metal_ore","value":30000},{"name":"acquired_metal.ore","value":32000},
            {"name":"acquired_scrap","value":900},{"name":"acquired_lowgradefuel","value":700},
            {"name":"harvest.humanmeat_raw","value":99999999},
            {"name":"comfort_duration","value":11709.4150390625},
            {"name":"kill_scientist","value":40},{"name":"kill_bear","value":10},
            {"name":"INVENTORY_OPENED","value":5}
        ]}})
    }

    #[test]
    fn raw_sorted_and_capped() {
        let raw = raw_stats(&sample()).unwrap();
        let keys: Vec<&String> = raw.keys().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
        assert_eq!(stats_json(&raw)["kill_player"], 67);
        assert!(stats_json(&raw)["comfort_duration"].is_f64());
        let many: Vec<Value> = (0..700).map(|i| json!({"name": format!("s{i:04}"), "value": i})).collect();
        let big = raw_stats(&json!({"playerstats":{"stats":many}})).unwrap();
        assert_eq!(big.len(), MAX_STATS);
        assert!(big.contains_key("s0000") && !big.contains_key("s0699"));
        assert!(raw_stats(&json!({"playerstats":{"error":"x"}})).is_none());
    }

    #[test]
    fn derived_and_weapons() {
        let d = derive(&raw_stats(&sample()).unwrap());
        assert_eq!(d["kills"], 67);
        assert_eq!(d["kdr"], 0.55);
        assert_eq!(d["weapons"]["bullet"]["accuracyPct"], 15.0);
        assert_eq!(d["weapons"]["arrow"]["accuracyPct"], 25.0);
        assert_eq!(d["weapons"]["shotgun"]["accuracyPct"], 0.0);
        // (900+100+0)/(6000+400+100) = 15.38 -> 15.4
        assert_eq!(d["shotsFired"], 6500);
        assert_eq!(d["shotsHit"], 1000);
        assert_eq!(d["accuracyPct"], 15.4);
        // headshot (150) имеет приоритет над headshots (40): 150/1000
        assert_eq!(d["headshots"], 150);
        assert_eq!(d["headshotPct"], 15.0);
        assert!(d["hitZones"].is_null());
        let r = &d["radar"];
        assert!(r["aim"].as_i64().unwrap() > 0);
        assert!(r["building"].is_null());
        assert!(r["pve"].as_i64().unwrap() > 0);
    }

    #[test]
    fn headshots_fallback_and_bad_class() {
        let mut raw = BTreeMap::new();
        raw.insert("headshots".to_string(), 5.0);
        raw.insert("bullet_fired".to_string(), 100.0);
        raw.insert("bullet_hit_player".to_string(), 50.0);
        raw.insert("shotgun_fired".to_string(), 10.0);
        raw.insert("shotgun_hit_player".to_string(), 40.0); // дробины: hit > fired, класс исключается
        let d = derive(&raw);
        assert_eq!(d["headshots"], 5);
        assert!(d["weapons"]["shotgun"]["accuracyPct"].is_null());
        assert_eq!(d["accuracyPct"], 50.0);
        assert_eq!(d["headshotPct"], 10.0);
    }

    #[test]
    fn body_silhouette() {
        let d = derive(&raw_stats(&sample()).unwrap());
        // fired 6000, hit 900, headshot 150
        assert_eq!(d["body"]["headPct"], 16.7);
        assert_eq!(d["body"]["bodyPct"], 83.3);
        assert_eq!(d["body"]["missPct"], 85.0);
        assert_eq!(d["body"]["fired"], 6000);
        assert_eq!(d["body"]["hit"], 900);
        assert_eq!(d["body"]["headshots"], 150);
        let mk = |f: f64, h: f64, s: Option<f64>| {
            let mut m = BTreeMap::new();
            m.insert("bullet_fired".to_string(), f);
            m.insert("bullet_hit_player".to_string(), h);
            body(&m, s)
        };
        // нули
        assert!(mk(0.0, 0.0, Some(0.0)).is_null());
        let z = mk(10.0, 0.0, Some(0.0));
        assert!(z["headPct"].is_null() && z["bodyPct"].is_null());
        assert_eq!(z["missPct"], 100.0);
        // hit > fired
        assert_eq!(mk(10.0, 20.0, Some(5.0))["missPct"], 0.0);
        // headshots > hit
        let b = mk(100.0, 10.0, Some(30.0));
        assert_eq!(b["headPct"], 100.0);
        assert_eq!(b["bodyPct"], 0.0);
        // нет данных
        assert!(derive(&BTreeMap::new())["body"].is_null());
        assert!(body(&BTreeMap::new(), Some(1.0)).is_null());
    }

    #[test]
    fn resources_dedupe_prefix_and_cap() {
        let res = resources(&raw_stats(&sample()).unwrap());
        let names: Vec<&str> = res.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(names, vec!["wood", "stone", "metal", "scrap", "oil"]);
        assert_eq!(res[0].1, 90397.0);
        assert_eq!(res[1].1, 51000.0);
        assert_eq!(res[2].1, 32000.0);
        let many: Vec<Value> = ["harvest.wood", "harvest.stones", "harvest.metal_ore", "harvest.sulfur_ore", "harvest.cloth",
            "harvest.fat_animal", "harvest.bone_fragments", "harvested_leather", "acquired_scrap", "acquired_lowgradefuel"]
            .iter()
            .enumerate()
            .map(|(i, n)| json!({"name": n, "value": 10 + i}))
            .collect();
        let r = resources(&raw_stats(&json!({"playerstats":{"stats":many}})).unwrap());
        assert_eq!(r.len(), MAX_RESOURCES);
    }

    #[test]
    fn empty_and_partial_are_null() {
        let d = derive(&BTreeMap::new());
        assert!(d["kills"].is_null() && d["kdr"].is_null() && d["accuracyPct"].is_null() && d["hitZones"].is_null());
        assert_eq!(d["playerType"], "unknown");
        assert!(d["radar"]["pvp"].is_null() && d["radar"]["gathering"].is_null());
        assert_eq!(d["resources"], json!([]));
        assert!(d["weapons"]["bullet"]["fired"].is_null());
        let mut raw = BTreeMap::new();
        raw.insert("kill_player".to_string(), 10.0);
        let d = derive(&raw);
        assert!(d["kdr"].is_null());
        assert!(d["radar"]["pvp"].as_i64().is_some());
    }

    #[test]
    fn building_and_survival_axes() {
        let mut raw = BTreeMap::new();
        raw.insert("placed_blocks".to_string(), 3000.0);
        raw.insert("calories_consumed".to_string(), 100000.0);
        let d = derive(&raw);
        assert_eq!(d["radar"]["building"], 50);
        assert_eq!(d["radar"]["survival"], 50);
    }

    #[test]
    fn sat_function() {
        assert_eq!(sat(100.0, 100.0), 50.0);
        assert_eq!(sat(0.0, 100.0), 0.0);
        assert!(sat(1e12, 100.0) < 100.0);
    }

    #[test]
    fn player_types() {
        let base = Radar::default();
        assert_eq!(player_type(&base), "unknown");
        assert_eq!(player_type(&Radar { pvp: Some(10.0), ..base.clone() }), "unknown");
        assert_eq!(player_type(&Radar { pvp: Some(70.0), pve: Some(20.0), ..base.clone() }), "pvp_roamer");
        assert_eq!(player_type(&Radar { gathering: Some(80.0), pvp: Some(30.0), ..base.clone() }), "farmer");
        assert_eq!(player_type(&Radar { building: Some(60.0), survival: Some(40.0), ..base.clone() }), "builder");
        assert_eq!(player_type(&Radar { pve: Some(60.0), ..base.clone() }), "pve_hunter");
        assert_eq!(player_type(&Radar { survival: Some(50.0), ..base.clone() }), "survivor");
        assert_eq!(player_type(&Radar { pvp: Some(50.0), gathering: Some(54.0), ..base }), "balanced");
    }
}
