# Доработки форка SteelMorgan/web-transport-addin

> Документ описывает доработки относительно upstream-проекта `alkoleft/web-transport-addin`. Форк построен поверх upstream `main` на ревизии 0.6.4 и реализует транспортный слой для интеграции с **v8-session-manager**. Текущая версия компоненты — **0.7.2** (`Manifest.xml`, `Cargo.toml`).
>
> **Этапы А/Б/В рефакторинга «transport-only» завершены (ADR-0005 accepted, 2026-05-05).** Прикладные обязанности (process supervision, эвристика прикладного `kind` по флагам платформы) вынесены из транспорта в BSL и в расширение `test_client` соответствующего форка `onec-client-mcp-devkit`. Это историческая запись о доработках; раздел «Технический долг» закрыт.

## Назначение доработок

Upstream-компонента предоставляла три класса для 1С: `ws` (WebSocket-клиент), `http` (HTTP/SSE-сервер) и `mcp` (MCP Streamable HTTP-сервер). В форке добавлен четвёртый класс — `session` — реализующий сессионный WS-клиент к внешнему оркестратору `v8-client-session-manager`. Это позволяет агрегировать MCP-tools со многих 1С-клиентов в едином каталоге и публиковать их AI-агенту через единую точку входа.

## Новый класс `AddIn.WebTransport.session`

Полный пайплайн «1С-клиент ↔ session-manager» через WebSocket. Реализован послойно (этапы 5–6), каждый слой покрыт unit-тестами (≥129 lib тестов).

| Файл | Назначение |
|------|-----------|
| `src/addin_host.rs` | trait `AddinHost` + `RealAddinHost` (поверх `addin1c::ExternalEvent`) + `MockAddinHost` для тестов. Изоляция от FFI — позволяет гонять цепочку без живой 1С (ADR-0004). |
| `src/session_params.rs` | Резолвер параметров сессии (`manager_url`, `kind`, `client_uid`, `correlation_id`) по контракту ADR-0020. `kind` приходит явным `/C "kind=..."`; если не передан — fallback `"client"`. *Эвристика `infer_kind` по флагам платформы (TESTMANAGER/TESTCLIENT/RunYaXUnit) удалена в этапе В как нарушение transport-only (ADR-0005); теперь это решает BSL.* |
| `src/tunnel.rs` | Generic duplex WS-pump (`Stream<TextOrClose>` + `Sink<String>`). `RunOutcome { Cancelled / Closed / InboundError / OutboundDropped / SinkError }`. Переполнение очереди 1С НЕ разрывает соединение. |
| `src/reconnect.rs` | Авто-reconnect с экспоненциальным backoff (`BackoffPolicy { initial, max, multiplier, max_attempts }`). Публикация состояний через `WS_RECONNECT_STATE` (`connecting` / `connected` / `disconnected` / `give_up`). |
| `src/session_integration.rs` | Высокоуровневый фасад `start / send / shutdown` поверх `tokio_tungstenite`. `WsConnector` + адаптеры `WsStreamAdapter` / `WsSinkAdapter`. Сторона 1С сама шлёт `session.register` после `WS_RECONNECT_STATE=connected` (список tools знает только 1С). |
| `src/session/addin.rs`, `src/session/mod.rs` | FFI-класс `session` для 1С: методы `start / send / stop / getParams`. |
| ~~`src/system_capability.rs`~~ | **Удалено в этапе В (ADR-0005).** Содержал JSON-RPC handlers `addin.spawn/addin.kill` и process supervisor поверх `tokio::process::Command` + `nix::signal` / `windows-sys`. Перенесено в `onec-client-mcp-devkit` → `exts/test_client/` как обычные MCP-tools `system_spawn_1c_client` / `system_kill_pid` (парный ADR-0003). ADR-0027 переведён в `superseded`. |
| `src/harness_tests.rs` | End-to-end тесты против настоящего WS-сервера (`tokio_tungstenite::accept_async`): входящие фреймы → `WS_INCOMING`, исходящие через `send()`, `start_correlated`, give-up по `max_attempts`. |

## Расширение существующих модулей

- `src/lib.rs` — регистрация класса `session`, инициализация `tracing` через `OnceLock` в `GetClassObject` (управляется `WEBTRANSPORT_LOG`, файл по умолчанию `/tmp/web-transport.log`).
- `src/ws_client.rs`, `src/mcp/server.rs` — точечные правки совместимости.
- `Cargo.toml` — добавлены `tokio-tungstenite`, `tracing`, `tracing-subscriber`, `uuid` v4. *После этапа В: зависимости `nix` и process-related часть `windows-sys` удалены вместе с `system_capability.rs`.*
- `.cargo/config.toml` — настройки кросс-компиляции (5 целей: Win x32/x64 mingw, Linux x32/x64, macOS x64 zigbuild).

## Системные параметры сессии (ADR-0029)

`SessionParams` несёт `host_id` и `pid` через trait `HostInfoProvider` (`OsHostInfo`: `V8_HOST_ID` env → `gethostname()` → `"unknown"`). *Поле `capabilities` (`["spawn","kill"]`) удалено в этапе В: маршрутизация на стороне менеджера идёт по имени MCP-tool, а не по флагу capability клиента (ADR-0005).*

## Override `client_uid` через `/C` (этап 6.6)

Manager-spawned клиенты получают expected `client_uid` через `/C "client_uid=..."`. Этот override побеждает 1С-генерируемый UUID в `resolve()` — без него reservation matching по uid в менеджере не работал.

## Tracing

Структурированное логирование добавлено во все слои: `session::addin` (start/stop/send), `session_integration::WsConnector` (connect_async, статус upgrade), `reconnect` (смена состояния), `addin_host::RealAddinHost` (каждый external_event с preview payload). Уровень — `WEBTRANSPORT_LOG` (default `info`), путь — `WEBTRANSPORT_LOG_FILE`.

## ADR

- `docs/decisions/0004-mock-strategy-for-addin-host-tests.md` — стратегия мока хоста для unit-тестов tunnel/reconnect/session_integration.

## Распределение ответственности: установка соединения и пинги

Связано с парным форком `SteelMorgan/onec-client-mcp-devkit` — расширение 1С (BSL) поверх этой компоненты.

### Установка соединения

| Слой | Файл | Что делает |
|------|------|-----------|
| **Rust addin (низ)** | `src/session_integration.rs:145` (`WsConnector::connect`) | `tokio_tungstenite::connect_async(url)` с таймаутом 5 сек, возвращает (Stream, Sink). |
| **Rust addin (orchestrator)** | `src/reconnect.rs` (`run_with_reconnect`) | Auto-reconnect с экспоненциальным backoff. Публикует `WS_RECONNECT_STATE = connecting / connected / disconnected / give_up` в 1С через `AddinHost`. |
| **Rust addin (фасад)** | `src/session_integration.rs:105` (`SessionIntegration::start`) | Высокоуровневый API. Принимает URL → создаёт `WsConnector` → запускает `reconnect`+`tunnel`. |
| **FFI-обёртка** | `src/session/addin.rs:176` | Метод `ЗапуститьСессионнуюИнтеграцию(URL, ClientUID, Kind, CorrelationID)` для 1С. |
| **BSL (точка входа)** | в `onec-client-mcp-devkit/.../Мсп_ТранспортСессионКлиент.Запустить` | Подключает компоненту, создаёт `AddIn.WebTransport.session`, дёргает `Компонента.ЗапуститьСессионнуюИнтеграцию(...)`. Реальное «connected» приходит асинхронно событием `WS_RECONNECT_STATE`. |

### Пинги

Используется **только** WS protocol-level Ping/Pong (RFC 6455).

- Кто инициирует: `v8-session-manager` шлёт WS Ping (opcode 0x9) каждые `mcp.session_manager.ws_ping_interval_ms` (default 20000 мс) из writer-task `run_connection`. Если за `ws_ping_timeout_ms` (default 30000 мс) от клиента не пришло ни одного фрейма (Pong/Text) — менеджер закрывает соединение, через `reconnection_grace_secs` запись удаляется. `ws_ping_interval_ms = 0` отключает Ping.
- Кто отвечает: `tokio_tungstenite` внутри addin — Pong возвращается автоматически на уровне WebSocket-обёртки, до того как фрейм доходит до приложения. См. `session_integration.rs:202-227` — Ping/Pong/Frame/Binary в стрим-адаптере игнорируются (Pong уже ушёл).
- Что детектит: разрыв TCP, NAT-timeout, half-close, dead peer на уровне сетевого стека. Не зависит от состояния BSL event-loop.

> **Application-level JSON-RPC ping в форке не используется.** Открытая модалка 1С (`Вопрос(...)`, `ОткрытьФормуМодально`) или длинный серверный запрос временно блокируют BSL event-loop, но это **легитимные пользовательские состояния**, а не «зависание». TCP/WS канал в этот момент жив (tokio worker addin'а отвечает Pong), и менеджер не должен ложно сбрасывать такие сессии. Если нужна именно прикладная reachability — реализуйте отдельным MCP-tool'ом, не путая с liveness канала.

### session.register после connect

BSL-сторона (`Мсп_ТранспортСессионКлиент.Module.bsl:185-226`) — обработчик `WS_RECONNECT_STATE = connected` сам собирает каталог tools/resources/prompts (snake_case wire format) и шлёт `session.register` менеджеру. Список знает только 1С, поэтому Rust в этом не участвует — комментарий в `session_integration.rs`: *«1С-код сам отвечает за session.register после WS_RECONNECT_STATE=connected»*.

## TL;DR ответственности

- **WS-handshake + auto-reconnect + WS Pong (auto)** → Rust addin (этот репозиторий).
- **WS Ping initiator + Pong-watchdog** → `1c-neurofish/v8-session-manager`.
- **session.register** → BSL (`SteelMorgan/onec-client-mcp-devkit`).
- Application-level ping не используется (см. выше).

## История: рефакторинг «transport-only» (этапы А/Б/В)

По итогам архитектурного ревью 2026-05-04 в форке зафиксированы нарушения принципа «transport-only»: addin кроме транспорта брал на себя прикладные обязанности (process supervisor для `addin.spawn`/`addin.kill`, `kind`-эвристика по флагам `СтрокаЗапуска()`). Полный анализ — в [ADR-0005: Transport-only Rust](docs/decisions/0005-transport-only-rust.md). Парный ADR на стороне прикладного расширения — `onec-client-mcp-devkit/docs/decisions/0003-spawn-tools-in-test-client.md`. Все три этапа выполнены и приняты к 2026-05-05; раздел сохранён как историческая справка о форк-эволюции.

### Что было исправлено

| # | Нарушение | Где (исторически) | Что сделано |
|---|-----------|-------------------|-------------|
| 1 | `kind`-эвристика по `СтрокаЗапуска()` — прикладной домен в transport-слое | `src/session_params.rs::infer_kind` | Эвристика удалена. `kind` приходит явным `/C "kind=..."`; иначе fallback `"client"`. Прикладное определение — в BSL `Мсп_ПараметрыЗапускаКлиент`. |
| 2 | `addin.spawn` / `addin.kill` JSON-RPC handlers + process supervisor — application capability в transport-компоненте | `src/system_capability.rs`, `src/tunnel.rs::try_dispatch_addin_method` | Удалены полностью (~840 строк + интеграционные тесты + диспатчер `addin.*` в `tunnel.rs`). Реализовано как MCP-tools `system_spawn_1c_client` / `system_kill_pid` в `onec-client-mcp-devkit/exts/test_client/`. Зависимости `nix`, process-related часть `windows-sys` удалены. |
| 3 | JSON-конверт `correlation_id` в incoming-payload | `src/tunnel.rs::dispatch_incoming_correlated` | Оставлен — необходимый инфраструктурный механизм трассировки реконнектов. |

### Хронология этапов

1. **Этап А (`onec-client-mcp-devkit`):** реализованы `system_spawn_1c_client` / `system_kill_pid` как MCP-tools в `exts/test_client/` (allow-list + regex-валидация). Старые `addin.spawn`/`addin.kill` в Rust сохранялись параллельно для обратной совместимости.
2. **Этап Б (`v8-session-manager`):** менеджер переключён с `addin.spawn`/`addin.kill` на новые MCP-tools. Жизненный цикл клиента отслеживается heartbeat'ом (timeout на `ping`).
3. **Этап В (этот репозиторий):** удалены `system_capability.rs`, `try_dispatch_addin_method`, supervisor registry, поле `capabilities` в `SessionParams`, эвристика `infer_kind`. ADR-0027 переведён в `superseded`. ADR-0029 (host_id/pid/capabilities) частично остаётся в силе: `host_id`/`pid` сохраняются в Rust, `capabilities` упразднены.

### Что **не** менялось

- Транспорт (`tunnel`, `reconnect`, `session_integration`).
- FFI-класс `session` (`session/addin.rs`).
- ADR-0004 (mock-strategy).
- correlation_id-конверт (нарушение 3 признано допустимым и сохранено).
