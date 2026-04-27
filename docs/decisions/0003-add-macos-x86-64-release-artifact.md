# ADR-0003: Добавить macOS x86_64 в release bundle внешней компоненты

- Статус: accepted
- Дата: 2026-04-27
- Приняли решение: maintainer, architecture owner
- Теги: release, macos, native-addin, packaging

## Контекст

`webtransport` поставляется как native add-in для 1С:Предприятия. До этого решения
release bundle собирал артефакты только для Windows и Linux:

- `WebTransportAddIn_x32-{version}.dll`
- `WebTransportAddIn_x64-{version}.dll`
- `WebTransportAddIn_x32-{version}.so`
- `WebTransportAddIn_x64-{version}.so`

Внешний пример из
[onec-client-mcp-devkit PR #2](https://github.com/1c-neurofish/onec-client-mcp-devkit/pull/2)
показал рабочую модель для macOS: собрать `libwebtransport.dylib` под
`x86_64-apple-darwin`, добавить его в архив компоненты и зарегистрировать в
`Manifest.xml` как `os="MacOS"` / `arch="x86_64"`.

Триггер для решения:

- 1С:Предприятие на macOS x86-64 может загрузить native add-in из `dylib`;
- текущий шаблон `Manifest.xml` не содержит macOS component entry;
- `scripts/dev-build.sh` уже копирует `libwebtransport.dylib` на Darwin, но затем
  ожидает Linux `.so` при упаковке demo-шаблона;
- отдельный PR-скрипт, который клонирует исходный репозиторий заново, не подходит для
  этого проекта, потому что source-of-truth сборки уже находится в текущем repo и
  `cargo make pack`.

Ограничения:

- не добавлять новый внешний build pipeline поверх существующего `cargo make pack`;
- сохранить текущие Windows/Linux артефакты и имена файлов;
- поддержать только macOS x86_64, потому что пример и проверенная 1С-платформа относятся
  к x86-64 client runtime;
- не заявлять поддержку `aarch64-apple-darwin`, пока нет проверенного runtime-сценария 1С;
- оставить bundle одним ZIP-архивом с единым `Manifest.xml`.

## Решение

Release bundle ДОЛЖЕН включать macOS x86_64 артефакт:

- файл: `WebTransportAddIn_x64-{version}.dylib`;
- Rust target: `x86_64-apple-darwin`;
- исходный build output: `target/x86_64-apple-darwin/release/libwebtransport.dylib`;
- manifest entry:
  `<component os="MacOS" path="WebTransportAddIn_x64-{version}.dylib" type="native" arch="x86_64" />`.

Шаблонный [Manifest.xml](/home/alko/develop/open-source/websocket1c/Manifest.xml)
ДОЛЖЕН также содержать macOS x64 запись без версии в имени:

`<component os="MacOS" path="WebTransportAddIn_x64.dylib" type="native" arch="x86_64" />`.

`scripts/dev-build.sh` ДОЛЖЕН упаковывать host-specific binary:

- Linux: `WebTransportAddIn_x64.so`;
- Darwin: `WebTransportAddIn_x64.dylib`;
- other host: `WebTransportAddIn_x64.dll`.

## Последствия

Положительные:

- macOS x86-64 1С client получает стандартный component entry в том же bundle, что и
  Windows/Linux;
- release pipeline остаётся единым и генерирует один `out/Manifest.xml`;
- dev-build на Darwin перестаёт падать на ожидании Linux `.so`;
- источник сборки остаётся текущим репозиторием, без временного клонирования внешнего
  GitHub repository.

Отрицательные:

- сборка полного release bundle теперь требует настроенного target
  `x86_64-apple-darwin` и совместимого linker/toolchain;
- Linux/Windows окружения без macOS cross-linker не смогут собрать полный bundle без
  дополнительной настройки CI или toolchain;
- macOS arm64 не покрыт этим решением.

Риски и меры:

- Риск: agents добавят отдельный скрипт, который пересобирает компоненту из другого repo.
  Мера: ADR фиксирует существующий `cargo make pack` как единственный release pipeline.
- Риск: manifest будет обновлён только в шаблоне или только в generated output.
  Мера: проверка должна покрывать и [Manifest.xml](/home/alko/develop/open-source/websocket1c/Manifest.xml),
  и генерацию `out/Manifest.xml` в [Makefile.toml](/home/alko/develop/open-source/websocket1c/Makefile.toml).
- Риск: случайно заявить arm64 support без проверки на стороне 1С.
  Мера: не добавлять `aarch64-apple-darwin` до отдельного решения и runtime-проверки.

## Рассмотренные альтернативы

### Оставить macOS support внешним patch-скриптом

Отклонено.

PR-пример использует отдельный `scripts/build-macos-addin.sh`, который клонирует source repo,
собирает dylib и модифицирует готовый `Template.addin`. Для этого проекта такая модель
дублирует release pipeline и создаёт второй источник истины.

### Добавить macOS только в demo/dev-build

Отклонено.

Это помогло бы локальной проверке на Darwin, но release ZIP по-прежнему не содержал бы
macOS component entry и `.dylib`.

### Сразу добавить macOS arm64

Отклонено.

Запрошенный и проверенный сценарий относится к 1С:Предприятие MacOS x86-64. Поддержка arm64
требует отдельной runtime-проверки и не должна появляться как неподтверждённая запись в
manifest.

## Не-цели

- менять runtime API компоненты;
- менять MCP/HTTP/WebSocket behavior;
- добавлять новый CI provider или отдельный pipeline orchestration;
- добавлять `aarch64-apple-darwin`;
- менять имена существующих Windows/Linux артефактов.

## План реализации

Затрагиваемые пути:

- `Makefile.toml`
- `Manifest.xml`
- `scripts/dev-build.sh`
- `docs/architecture.md`
- `docs/architecture/arc42/architecture.md`
- `docs/decisions/README.md`

Шаги реализации:

1. Добавить Rust target `x86_64-apple-darwin` в `install-targets`; для macOS host также
   оставить targets Windows GNU и Linux GNU, которые нужны полному bundle.
2. Добавить cargo-make task `build-release-macos-64` для `x86_64-apple-darwin`.
3. Включить `build-release-macos-64` в `tasks.release`.
4. В `pack-to-zip` для shell/macOS shell и PowerShell:
   - скопировать `target/x86_64-apple-darwin/release/libwebtransport.dylib`;
   - переименовать его в `WebTransportAddIn_x64-{version}.dylib`;
   - добавить macOS запись в generated `out/Manifest.xml`;
   - включить `.dylib` в ZIP.
5. Добавить macOS x64 запись в шаблонный `Manifest.xml`.
6. Исправить `scripts/dev-build.sh`, чтобы update demo template выбирал binary по host OS.
7. Обновить архитектурную документацию и индекс ADR.

Паттерны, которых нужно избегать:

- не клонировать этот же проект во временную директорию ради сборки macOS dylib;
- не патчить готовый `Template.bin` вручную как основной release path;
- не добавлять arm64 target без отдельного runtime подтверждения;
- не оставлять generated manifest и шаблонный manifest с разным списком OS.

Конфигурационные изменения:

- разработчик или CI должен установить target:
  `rustup target add x86_64-apple-darwin`;
- для non-macOS host может потребоваться отдельный cross-linker/toolchain.

## Проверка

- [ ] `Manifest.xml` содержит `<component os="MacOS" path="WebTransportAddIn_x64.dylib" type="native" arch="x86_64" />`.
- [ ] `Makefile.toml` содержит `x86_64-apple-darwin` в install-targets.
- [ ] `tasks.release` включает `build-release-macos-64`.
- [ ] `pack-to-zip` генерирует macOS запись в `out/Manifest.xml`.
- [ ] `pack-to-zip` включает `WebTransportAddIn_x64-{version}.dylib` в ZIP.
- [ ] `scripts/dev-build.sh` на Darwin упаковывает `WebTransportAddIn_x64.dylib`, а не Linux `.so`.
- [ ] `cargo check` проходит для основного host target.
- [ ] Полная macOS release-сборка проверяется на host/CI с настроенным `x86_64-apple-darwin` linker.

## Связанные документы

- [onec-client-mcp-devkit PR #2](https://github.com/1c-neurofish/onec-client-mcp-devkit/pull/2)
- [Индекс ADR](/home/alko/develop/open-source/websocket1c/docs/decisions/README.md)
- [Архитектурная документация arc42](/home/alko/develop/open-source/websocket1c/docs/architecture/arc42/architecture.md)
