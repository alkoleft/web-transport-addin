# ADR-0004: Стратегия мока хоста внешней компоненты для тестов tunnel

- Статус: accepted
- Дата: 2026-04-28
- Приняли решение: maintainer, architecture owner
- Теги: testing, native-addin, addin1c, ws-tunnel, session-manager

## Контекст

Этап 5 backlog'а `v8-client-session-manager` требует от компоненты `webtransport`
двух новых способностей:

- двунаправленный WS-tunnel: компонент должен не только слать в session-manager
  ответы 1С-клиента, но и принимать `tool.call` оттуда и поднимать их в 1С через
  `external_event`;
- автоматический reconnect с backoff и публикация состояния канала событием
  `WS_RECONNECT_STATE`.

Существующие реализации (`src/mcp/server.rs`, `src/http/server.rs`,
`src/http/mcp_handler.rs`) дёргают `connection.external_event(...)` напрямую,
где `connection: &'static addin1c::Connection`. Этот тип создаётся только
платформой 1С при загрузке native add-in; в Rust-тесте получить его без живой
1С нельзя. Из-за этого ни одну ветку логики, которая порождает событие в 1С,
сегодня нельзя проверить unit-тестом — она просто `return Err(...)` через
`if let Some(connection) = self.connection else { ... }`.

Цель этого ADR — зафиксировать решение, как тестировать новый WS-tunnel и
существующие event-эмиттеры без реальной информационной базы. Это явное
требование из этапа 5 backlog'а:

> «можем ли обернуть внешнюю компоненту каким-то моком, будто она внутри
> расширения 1С, чтобы тестировать её взаимодействие с менеджером сессий без
> прямого подключения к базе».

## Рассмотренные альтернативы

### A. Поднимать живой 1С-сервер в CI

Отклонено. Тяжеловесная инфраструктура, лицензионные и платформенные
ограничения, медленные тесты. Этап 5 ещё не должен зависеть от живой DRIVE.

### B. Тестировать только поведение session-manager, компонент оставить без
unit-покрытия

Отклонено. WS-tunnel и reconnect живут именно в компоненте; без её локального
покрытия мы получим серый ящик и регресс будет ловиться только сценарными
тестами в DRIVE на этапе 6.

### C. Делать `Connection` параметризованным generic'ом во всех типах

Отклонено. `addin1c::Connection` не отдаёт публичных trait'ов, generic
протекает в форму `MCP/HTTP/WS Addin`, увеличивает шум в API без выгоды.

### D. Ввести небольшой trait-абстрагирующий слой `AddinHost` поверх
`Connection.external_event`

Принято.

## Решение

В компоненте появляется новый внутренний модуль `addin_host` с тремя сущностями:

- trait `AddinHost: Send + Sync` с единственным методом
  `external_event(&self, event: &str, payload: &str) -> bool`;
- `RealAddinHost` — обёртка над `&'static addin1c::Connection`, в production
  используется в загруженном в 1С компоненте;
- `MockAddinHost` — in-memory реализация для тестов: накапливает вызовы как
  `(event, payload)`, поддерживает имитацию переполнения очереди, потокобезопасна.

Контракт совпадает с существующим вызовом `connection.external_event(name!("WebTransport"), event, data)`:
имя компоненты `WebTransport` фиксируется внутри `RealAddinHost` и в
константе `addin_host::COMPONENT_NAME`. Имя события и UTF-8 payload приходят
снаружи. UTF-16 преобразование (`CString1C`) выполняет только `RealAddinHost` —
для теста это деталь реализации Real-импла.

Использование на этапе 5:

- `tunnel`-цикл (5.3): вход WS → парсинг JSON-RPC → `host.external_event("WS_INCOMING", payload)`;
- reconnect-логика (5.4): смена состояния → `host.external_event("WS_RECONNECT_STATE", payload)`;
- `addin_harness` (5.7): запускает реальный session-manager и подменяет `MockAddinHost`,
  чтобы acceptance-тесты могли проверять корректность доставки `tool.call` в «1С»
  без 1С.

Существующие call-site'ы (`mcp/server.rs`, `http/server.rs`, `http/mcp_handler.rs`)
переключаются на `AddinHost` поэтапно при внедрении WS-tunnel; внутри 5.1
изменения аддитивны и публичные интерфейсы 1С не трогают.

## Последствия

Положительные:

- WS-tunnel и reconnect получают unit и acceptance-покрытие без живой 1С;
- единая точка эмиссии событий упрощает добавление correlation_id и
  телеметрии (этап 7);
- explicit boundary между «UTF-8 в логике» и «UTF-16 в FFI», которая раньше
  была размазана по сайтам.

Отрицательные:

- ещё один слой абстракции в небольшом компоненте — стоимость должна окупаться
  тестами этапа 5; если нет, ADR пересмотрим.
- `RealAddinHost` хранит `&'static Connection` — это совместимо с тем, как
  `addin1c` уже отдаёт connection, но требует контроля времени жизни в будущих
  изменениях.

Риски и меры:

- Риск: разные импликации поведения Real и Mock (например, async-доставка
  событий в 1С) приведут к ложно-зелёным тестам. Мера: на этапе 5.7 (harness)
  обязательное end-to-end покрытие через реальный session-manager + Mock,
  где проверяется не только факт вызова, но и порядок событий.
- Риск: имя компоненты `WebTransport` зашьётся в нескольких местах. Мера:
  единственным источником становится константа `addin_host::COMPONENT_NAME`.

## Не-цели

- менять публичный API классов `ws`, `http`, `mcp` для 1С;
- переписывать существующие реализации эмиттеров событий (это сделают
  подзадачи 5.3+ при включении tunnel);
- вводить полноценный механизм логирования/телеметрии — это этап 7.

## План реализации

В рамках 5.1:

1. Создать `src/addin_host.rs` с `AddinHost`, `RealAddinHost`, `MockAddinHost`.
2. Подключить модуль из `src/lib.rs` (с `#[allow(dead_code)]` до 5.3).
3. Покрыть `MockAddinHost` unit-тестами: порядок событий, переполнение
   очереди, очистка буфера, `Send + Sync`-проверка, dyn-диспетчеризация.
4. Зафиксировать ADR и обновить индекс `docs/decisions/README.md`.

В рамках 5.3+:

5. Переключить `mcp::server`, `http::server`, `http::mcp_handler` на
   `AddinHost` (без изменения внешнего поведения).
6. WS-tunnel принимает `Arc<dyn AddinHost>` в конструкторе.
7. `addin_harness` использует `MockAddinHost` и проверяет доставку
   `tool.call` end-to-end.

## Проверка

- [x] Модуль `addin_host` собирается (`cargo build --lib`).
- [x] Unit-тесты `addin_host` зелёные (`cargo test --lib addin_host`).
- [x] Полный набор `cargo test --lib` остаётся зелёным.
- [ ] На этапе 5.3 существующие call-site'ы переключены на `AddinHost`,
      внешнее поведение 1С-клиента не изменилось.

## Связанные документы

- [ADR-0002: Отказаться от внутренней эмуляции задач в MCP bridge](0002-drop-internal-task-emulation-in-mcp-bridge.md)
- [Архитектурная документация arc42](../architecture/arc42/architecture.md)
- Backlog session-manager этапа 5 (`v8-client-session-manager/spec/IMPLEMENTATION_BACKLOG_SESSION_MANAGER.md`)
