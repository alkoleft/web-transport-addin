# ADR-0005: Transport-only Rust — убираем addin.spawn/kill и process supervisor

- Статус: accepted
- Дата: 2026-05-04 (proposed) → 2026-05-05 (accepted после реализации этапов А/Б/В).
- Связанные ADR: ADR-0027 переведён в `superseded`. ADR-0029 (host_id/pid) сохраняет силу частично — поле `capabilities` упразднено, host_id/pid остаются.
- Связанные документы: [`FORK_CHANGES.md`](../../FORK_CHANGES.md), парный ADR `onec-client-mcp-devkit/docs/decisions/0003-spawn-tools-in-test-client.md`.

## Контекст

В этапе 6 (см. коммит `99398e1`) в `web-transport-addin` была добавлена реализация JSON-RPC методов `addin.spawn` / `addin.kill` и process supervisor:

- `src/system_capability.rs` (~799 строк): handlers `addin.spawn` / `addin.kill` поверх `tokio::process::Command` + `nix::signal` (Linux) / `windows-sys` (Windows). Supervisor registry `Arc<Mutex<HashMap<pid, ChildHandle>>>` отслеживает дочерние процессы и шлёт `addin.child_exited` уведомления через WS.
- `src/tunnel.rs::try_dispatch_addin_method`: перехват `addin.*` методов до отправки в `WS_INCOMING`. Не-`addin` фреймы по-прежнему уходят в 1С.
- `SessionParams.capabilities = ["spawn", "kill"]` объявляется в `session.register`, чтобы менеджер знал, какие клиенты могут спавнить.

Решение «всё в Rust» было обусловлено тем, что:
- `tokio::process::Command` async-друживает с реактором transport'а;
- supervisor-task переживает reconnect (живёт в Rust-rt, не в BSL);
- ответ на `addin.spawn` шёл по тому же сокету без участия BSL.

После проведённого ревью архитектуры ([диалог 2026-05-04](#)) выявлено, что эти аргументы **не оправдывают** размещение прикладного слоя в transport-компоненте:

1. **Слой нарушен.** Принципиальная граница: `web-transport-addin` отвечает только за транспорт (WS pump, reconnect, FFI). Process spawn — это прикладная capability (system management), а не transport. Размещение её здесь смешивает слои.
2. **MCP catalog visibility.** `addin.spawn` живёт в приватном namespace `addin.*` и не виден стандартному MCP-клиенту через `tools/list`. AI-агент не может вызвать spawn напрямую — только через специальный код менеджера.
3. **Policy / audit / read-only mode** — естественно лежат в BSL (там есть пользовательский контекст 1С, ЖР, права доступа). В Rust — каждый раз костыль.
4. **PID не нужен на стороне Rust.** `host_id`/`pid` уже прокидываются через `session.register.params` (ADR-0029). Менеджер знает PID каждого подключённого клиента и без supervisor'а.
5. **Supervisor не нужен.** Жизненный цикл клиента отслеживается менеджером по heartbeat — отсутствие ответа на JSON-RPC `ping` в течение N сек = клиент мёртв. Это и так требуется для общего liveness'а; отдельный сигнал `addin.child_exited` дублирует функцию.

## Решение

Удалить из `web-transport-addin` всю функциональность spawn/kill/supervisor. Компонента становится **чисто транспортной**: WS pump, reconnect, FFI для отправки/приёма сообщений. Application layer (включая spawn-tools) переезжает в прикладное расширение `exts/test_client/` репозитория `onec-client-mcp-devkit`.

### Что удаляется

| Файл / модуль | Размер | Действие |
|----------------|--------|----------|
| `src/system_capability.rs` | ~799 строк | удалить целиком |
| `src/tunnel.rs::try_dispatch_addin_method` и весь префикс-перехват `addin.*` | ~50 строк | удалить, входящие фреймы всегда уходят в `WS_INCOMING` |
| Supervisor registry `Arc<Mutex<HashMap<pid, ChildHandle>>>` | (внутри `system_capability.rs`) | удалить вместе с файлом |
| Зависимости `Cargo.toml`: `nix` (Linux), `windows-sys::Threading`/`PROCESS_TERMINATE` | feature-gates | удалить |

### Что остаётся (без изменений)

| Файл / модуль | Назначение |
|----------------|-----------|
| `src/addin_host.rs` | trait `AddinHost` + Real/Mock — transport infrastructure |
| `src/session_params.rs` | резолвер params (manager_url, kind, client_uid, correlation_id, host_id, pid). **Эвристика `kind` по `СтрокаЗапуска()` уходит в BSL** — см. ADR в onec-client-mcp-devkit |
| `src/tunnel.rs` (без `try_dispatch_addin_method`) | duplex WS pump |
| `src/reconnect.rs` | exponential backoff, `WS_RECONNECT_STATE` events |
| `src/session_integration.rs` | facade `start/send/shutdown` |
| `src/session/addin.rs` | FFI-класс `session` (start/send/stop/getParams) |
| `src/harness_tests.rs` | интеграционные тесты transport'а |

### Capabilities — отдельный механизм отменяется

Изначально предполагалось, что Rust перестанет жёстко зашивать `["spawn","kill"]`, а BSL будет передавать capabilities через FFI. После повторного анализа выяснилось, что **отдельное поле `capabilities` вообще избыточно**: в `session.register.params.tools` уже идёт массив зарегистрированных в BSL инструментов с их именами. Менеджер использует capabilities ровно в одном месте — `registry::find_spawner(host_id, "spawn")` (`v8-client-session-manager/src/session_manager/registry.rs:323`), — и эта функция тривиально заменяется на поиск сессии, в чьём каталоге tools присутствует имя `system_spawn_1c_client`.

Решение для Rust:
- На этапе В удаляется как `vec!["spawn","kill"]` по умолчанию, так и само поле `SessionParams.capabilities` (вместе с сериализацией в `session.register`).
- BSL **ничего не передаёт** — capabilities как отдельной сущности не существует.
- Маршрутизация на стороне менеджера переключается на поиск по имени tool (см. парный ADR в onec-client-mcp-devkit).

### child_exited

Уведомление `addin.child_exited` исчезает. Менеджер обнаруживает мёртвого клиента через timeout на heartbeat (отсутствие ответа на JSON-RPC `ping` в течение настраиваемого окна, обычно 30 сек). См. парный ADR в onec-client-mcp-devkit.

## Последствия

### Положительные

- Размер `web-transport-addin` уменьшается на ~850 строк (system_capability.rs + куски tunnel.rs).
- Граница слоёв чистая: transport отдельно, application отдельно.
- AI-агент видит spawn в стандартном MCP `tools/list`.
- Policy / audit / read-only добавляются в BSL без перекомпиляции компоненты.
- Зависимости `nix` и часть `windows-sys` уходят — упрощается кросс-компиляция.

### Отрицательные

- **Breaking change wire-format.** Менеджер должен перестать слать `addin.spawn` / `addin.kill` и начать вызывать MCP `tool.call` с именами tools, которые регистрирует расширение `test_client`. Версионирование менеджера обязательно.
- **Спавн без живой BSL невозможен.** Сейчас Rust отвечает сам, без участия 1С. После рефакторинга: если BSL не загрузился (например, конфигурация повреждена) — spawn недоступен. На практике это не сценарий: мёртвый клиент не должен спавнить новых.
- **Latency спавна +5–20 мс** на дополнительный hop через `external_event` → BSL handler → FFI. Для операции, которая занимает 3–5 секунд (старт 1С-клиента) — в пределах шума.
- **Shell-injection поверхность переезжает в BSL.** В Rust `tokio::process::Command` принимает массив аргументов на уровне OS API (execve / CreateProcess). В BSL `ЗапуститьПриложение` принимает строку, которая парсится платформой. Защита — allow-list бинарников и regex-валидация значений (см. парный ADR).

### Риски

- Если кому-то в инфраструктуре потребуется general-purpose process spawn (произвольный binary, произвольные args), ему придётся либо реализовывать allow-list расширение в BSL, либо возвращать Rust-supervisor. Для проекта в текущей конфигурации это маловероятно.

## План перехода

Реализация поэтапная, синхронизированная между тремя репозиториями.

1. **Этап А (BSL).** В `onec-client-mcp-devkit/exts/test_client/` появляются tools `system_spawn_1c_client` / `system_kill_pid` с allow-list и regex-валидацией. Старые `addin.spawn` остаются в Rust до подтверждения паритета функциональности.
2. **Этап Б (Менеджер).** `v8-client-session-manager` переключается с `addin.spawn` / `addin.kill` на новые MCP-tools. Heartbeat-monitor добавляется как замена `child_exited`.
3. **Этап В (Rust).** После того, как менеджер перестал использовать `addin.*` в production, удаляется `system_capability.rs` и связанные куски. ADR-0027 переводится в `superseded`.

Каждый этап — отдельная PR-ветка в соответствующем репозитории. На промежуточных стадиях оба пути работают параллельно (graceful migration).

## Ссылки

- ADR-0027 (system capability layer) — будет `superseded` по завершении этапа В.
- ADR-0029 (host_id/pid/capabilities в session.register) — частично сохраняет силу: host_id и pid остаются в Rust; поле `capabilities` упраздняется как отдельный механизм (маршрутизация переходит на имена tools в `params.tools`).
- Парный ADR `onec-client-mcp-devkit/docs/decisions/0003-spawn-tools-in-test-client.md`.
- Дискуссия по архитектуре, диалог 2026-05-04.
