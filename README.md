# WebTransport 1C Addin (Rust)

Внешняя компонента для 1С, объединяющая WebSocket‑клиент и HTTP/SSE сервер с обменом событиями с 1С.

Проект основан на оригинальном репозитории и шаблоне внешней компоненты на Rust:
- Первоисточник: https://github.com/dlyubanevich/websocket1c
- Шаблон компоненты (Rust): https://github.com/medigor/addin1c

## Состав и имена классов

Компонента экспортирует следующие классы (имена для `Новый("AddIn.*")`):
- `ws` — WebSocket‑клиент. См. [docs/ws.md](docs/ws.md).
- `http` — HTTP/SSE сервер с событиями в 1С. См. [docs/http.md](docs/http.md).
- `mcp` — MCP Streamable HTTP сервер (JSON‑only). См. [docs/mcp.md](docs/mcp.md).

## Доработки форка

Этот форк отличается от upstream `alkoleft/web-transport-addin`. Описание доработок, перечень новых/изменённых модулей и распределение ответственности между слоями — в **[FORK_CHANGES.md](FORK_CHANGES.md)**.
