# hollow++

Десктопная панель для Rust+ на Tauri v2: Rust-бэкенд и один HTML-фронт (`ui/index.html`), без npm.

## Сборка
Нужны Rust и Visual Studio Build Tools (C++).

```
cargo install tauri-cli --version "^2" --locked
cd C:\Users\offen\hollow
cargo tauri dev      # запуск для разработки
cargo tauri build    # установщик NSIS в src-tauri\target\release\bundle\nsis
```

## Что где лежит
- `ui/index.html` - весь фронтенд (общается с бэкендом через `window.__TAURI__`).
- `src-tauri/src/lib.rs` - сборка приложения и регистрация команд.
- `src-tauri/src/profiles.rs` - пачки (профили), хранятся в `profiles.json` в каталоге данных приложения.
- `src-tauri/src/rustplus/` - клиент Rust+ (WebSocket + protobuf).
- `src-tauri/src/battlemetrics.rs` - BattleMetrics: поиск и слежение за игроками.
- `src-tauri/src/types.rs` - общие типы.
- `CONTRACT.md` - контракт между модулями.

## Пачки
Пачка - это набор настроек: сервер (ip, порт, SteamID64, player token) и токен BattleMetrics.
Можно создать несколько пачек, переименовать, изменить и удалить (последнюю удалить нельзя).
При переключении текущее подключение закрывается, слежение сбрасывается и открывается подключение
к серверу новой пачки. При первом запуске создаётся пустая «Моя пачка». Токены хранятся только
в `profiles.json` и в списке показываются маской.
