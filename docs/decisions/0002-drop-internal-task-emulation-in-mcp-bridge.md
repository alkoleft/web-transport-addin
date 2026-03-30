# ADR-0002: Отказаться от внутренней эмуляции задач в MCP bridge

- Статус: accepted
- Дата: 2026-03-30
- Приняли решение: maintainer, architecture owner
- Теги: mcp, tasks, streamable-http, simplification

## Контекст

[ADR-0001](/home/alko/develop/open-source/websocket1c/docs/decisions/0001-use-sse-transport-for-async-mcp-calls.md)
зафиксировал модель, в которой компонент для `taskSupport = "optional"` мог создавать
внутренние задачи, скрытые от публичного MCP Tasks API, и завершать их через transport
bridge на SSE.

Эта модель дала единый async-механизм внутри компоненты, но создала дополнительный слой
поведения поверх стандартного MCP:

- отдельную внутреннюю ветку исполнения для plain `tools/call`;
- специальные сущности и условия в Rust-коде (`internal:*`, private task lifecycle,
  дополнительные ветки маршрутизации);
- bridge-контракт с 1С, который должен различать не только стандартные MCP-сценарии,
  но и внутреннюю эмуляцию;
- документацию и тесты, описывающие поведение, которого нет в стандартной модели MCP tasks.

Триггер для нового решения:

- логика получилась переусложнённой;
- код и документация отклонились от стандартного MCP поведения;
- внутренняя эмуляция мешает раскрыть потенциал публичных MCP tasks как основной модели
  длительных операций;
- компонент должен стремиться быть thin bridge над Streamable HTTP и стандартным MCP, а не
  собственной orchestration-слойкой.

Ограничения и драйверы решения:

- сохранить совместимость со Streamable HTTP transport;
- не добавлять новую нестандартную execution-semantics вместо удаляемой;
- оставить `taskSupport = "optional"` стандартным режимом, где plain `tools/call`
  остаётся допустимым, а task flow используется только если его выбрал клиент;
- сделать так, чтобы решение можно было реализовать через упрощение
  [server.rs](/home/alko/develop/open-source/websocket1c/src/mcp/server.rs),
  а не через очередной compatibility layer.

Этот ADR supersedes ADR-0001.

## Решение

Компонента ДОЛЖНА отказаться от внутренней эмуляции задач для MCP tool calls.

Новая модель исполнения:

### `taskSupport = "forbidden"` или не задан

- Поддерживается только стандартный non-task `tools/call`.
- Компонента не создаёт задачи и не переводит такой вызов во внутренний async-flow.

### `taskSupport = "optional"`

- Plain `tools/call` остаётся допустимым стандартным путём.
- Публичный task flow через `tasks/*` используется только тогда, когда его выбрал сам клиент.
- Компонента не должна подменять plain `tools/call` внутренней задачей, скрытой от клиента.

### `taskSupport = "required"`

- Поддерживается только стандартный client-visible task flow.
- Plain `tools/call` должен отклоняться.

Дополнительные правила:

- Компонента не должна создавать private task entries, отсутствующие в публичном MCP Tasks API.
- Компонента не должна использовать префиксы вида `internal:` или эквивалентные private task id.
- Компонента не должна использовать `_meta.responseMode` как внутренний переключатель между
  sync/SSE/task ветками.
- Компонента не должна документировать `stream_sse` как отдельный component-specific способ
  асинхронного вызова инструмента.
- Bridge с 1С должен различать только стандартные сценарии:
  - обычный sync/non-task вызов;
  - публичную MCP задачу.

Transport-уровень:

- Компонента продолжает работать через Streamable HTTP transport.
- Это не означает сохранение отдельной внутренней execution-semantics поверх стандарта.
- Длительные операции должны раскрываться через стандартный MCP task lifecycle, если
  инструмент требует или использует task-based исполнение.

## Последствия

Положительные:

- уменьшается число веток маршрутизации, условий и специальных сущностей в bridge-коде;
- семантика `forbidden` / `optional` / `required` становится ближе к стандартному MCP;
- публичные MCP tasks становятся единственным task-механизмом компоненты;
- клиенту и 1С проще понимать поведение сервера;
- тесты и документация становятся короче и менее двусмысленными.

Отрицательные:

- клиенты, которые опирались на component-specific `_meta.responseMode = "stream_sse"` или
  на внутреннюю async-эмуляцию для plain `tools/call`, должны мигрировать;
- long-running обработчики, зарегистрированные как `optional`, больше не получают скрытый
  async-path автоматически;
- старый bridge-контракт с 1С, демо и документация потребуют синхронного обновления.

Риски и меры:

- Риск: частичное удаление internal-flow оставит мёртвые ветки и stale documentation.
  Мера: убирать internal task model целиком, а не маскировать её.
- Риск: старые клиенты будут продолжать передавать `_meta.responseMode = "stream_sse"`.
  Мера: перестать полагаться на это поле в маршрутизации и явно описать migration path
  в документации.
- Риск: `optional`-инструменты будут использоваться как long-running plain `tools/call`
  и упираться в timeout.
  Мера: такие сценарии должны либо укладываться в обычный request/response, либо быть
  переведены в явный task-based клиентский flow.

## Рассмотренные альтернативы

### Сохранить внутреннюю эмуляцию задач для `taskSupport = "optional"`

Отклонено.

Это и было решением ADR-0001, но практика показала, что оно создаёт избыточную сложность,
размывает стандартную семантику MCP tasks и перегружает bridge между Rust и 1С.

### Требовать публичный task flow для всех async-capable инструментов

Отклонено.

Это действительно упростило бы модель ещё сильнее, но уже вышло бы за рамки стандартной
семантики `taskSupport = "optional"` и убрало бы у клиента законный выбор plain `tools/call`.

### Оставить `_meta.responseMode` как legacy compatibility layer

Отклонено.

Это сохранило бы часть старого поведения, но не решило бы главную проблему: компонент
продолжал бы интерпретировать и поддерживать собственную execution-логику поверх стандарта.

## Не-цели

- изменение standalone HTTP/SSE модуля в `src/http/*`;
- удаление публичного MCP Tasks API;
- добавление нового component-specific async-механизма вместо внутренней эмуляции;
- изменение базового контракта регистрации инструментов за пределами стандартной
  семантики `taskSupport`;
- оптимизация производительности вне упрощения execution model.

## План реализации

Затрагиваемые пути:

- `src/mcp/server.rs`
- `src/mcp/addin.rs`
- `docs/mcp.md`
- `docs/architecture.md`
- `docs/architecture/arc42/architecture.md`
- `demo/Demo/Forms/Форма/Ext/Form/Module.bsl`
- `docs/decisions/0001-use-sse-transport-for-async-mcp-calls.md`
- `docs/decisions/README.md`

Шаги реализации:

1. Удалить internal task model из `src/mcp/server.rs`.
   - Убрать private task branches, скрытые task entries и идентификаторы с префиксом `internal:`.
   - Убрать код, который отличает публичные задачи от внутренних только ради эмуляции.

2. Упростить маршрутизацию tool calls.
   - Перестать использовать `_meta.responseMode` как часть execution routing.
   - Для plain `tools/call` оставить только стандартный non-task path.
   - Для task-based вызова оставить только стандартный публичный task path.

3. Упростить bridge-контракт с 1С.
   - Оставить в `MCP_TOOL_CALL` только данные, необходимые для различения:
     - sync/non-task вызова;
     - публичной MCP задачи.
   - Удалить из payload и документации поля и значения, существующие только для internal emulation.

4. Обновить поведение `taskSupport`.
   - `forbidden`: plain `tools/call`, без task flow.
   - `optional`: plain `tools/call` или client-selected `tasks/*`, без server-side подмены.
   - `required`: только client-selected `tasks/*`.

5. Обновить документацию и demo.
   - Удалить описание внутренней эмуляции задач.
   - Удалить описание `_meta.responseMode = "stream_sse"` как поддерживаемого execution режима.
   - Переписать demo 1С так, чтобы оно отражало только стандартные sync/public task сценарии.

6. Обновить тесты.
   - Удалить тесты, проверяющие internal task emulation.
   - Добавить/обновить тесты для стандартного поведения `forbidden`, `optional`, `required`.
   - Зафиксировать, что plain `optional` вызов не создаёт скрытую MCP-задачу.

Паттерны, которых нужно избегать:

- не вводить новый private async-flow под другим именем;
- не сохранять мёртвые перечисления, поля и условия “на будущее”;
- не оставлять `_meta.responseMode` как скрытый execution override;
- не маскировать внутреннюю задачу под публичную или наоборот.

Конфигурационные изменения:

- не требуются.

Миграция:

1. Клиенты, использующие `_meta.responseMode = "stream_sse"`, должны перейти либо на
   стандартный plain `tools/call`, либо на стандартный task-based flow.
2. Документация 1С должна перестать описывать `internal:` task ids и private task lifecycle.
3. Demo и автотесты должны быть синхронизированы в одном цикле изменений с серверной логикой.

## Проверка

- [ ] В runtime code, тестах и документации отсутствует модель `internal:*` задач.
- [ ] Компонента не создаёт private task entries, скрытые от MCP Tasks API.
- [ ] Plain `tools/call` для `taskSupport = "optional"` работает без server-side создания задачи.
- [ ] Task-based вызов для `taskSupport = "optional"` создаёт стандартную публичную MCP задачу.
- [ ] Plain `tools/call` для `taskSupport = "required"` отклоняется.
- [ ] Task-based вызов для `taskSupport = "required"` работает через стандартный публичный task flow.
- [ ] `taskSupport = "forbidden"` не допускает task-based execution.
- [ ] Компонента больше не использует `_meta.responseMode` для выбора execution path.
- [ ] `docs/mcp.md` и архитектурная документация описывают только стандартные sync/public task сценарии.
- [ ] Demo 1С отражает ту же модель, что и код.

## Связанные документы

- [ADR-0001: Использовать `text/event-stream` для асинхронных MCP-вызовов инструментов](/home/alko/develop/open-source/websocket1c/docs/decisions/0001-use-sse-transport-for-async-mcp-calls.md)
- [Индекс ADR](/home/alko/develop/open-source/websocket1c/docs/decisions/README.md)
- [Документация MCP-сервера](/home/alko/develop/open-source/websocket1c/docs/mcp.md)
- [Обзор архитектуры](/home/alko/develop/open-source/websocket1c/docs/architecture.md)
- [arc42 архитектурная документация](/home/alko/develop/open-source/websocket1c/docs/architecture/arc42/architecture.md)
