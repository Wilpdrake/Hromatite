# README_AI

Этот документ предназначен для ИИ-ассистентов, которые будут дорабатывать проект Hromatite. Следуй этим правилам перед любыми изменениями.

## Цель проекта

Hromatite — модульное Python-приложение с асинхронным ядром, Alembic-миграциями, SQLAlchemy-моделями и веб-админкой на FastAPI.

Основные точки входа:

- `main.py` — запуск всего приложения и регистрация включённых модулей.
- `run_web.py` — отдельный запуск web-админки.
- `core/` — конфиг, БД, DI, реестр модулей, логирование.
- `modules/web/` — web-модуль, API, сервисы, репозитории, модели, статика и шаблоны.
- `migrations/` — Alembic-миграции.
- `models/models.py` — совместимый фасад для Alembic и старых импортов.
- `docs/deploy/` — памятки и шаблоны для деплоя nginx, WireGuard и SSL.

## Архитектурные принципы

### KISS

Делай простые решения. Не добавляй абстракции, если нет минимум двух реальных мест использования.

### DRY

Не дублируй бизнес-логику. Общая логика должна жить в сервисах или маленьких helper-функциях.

### WET

Не делай преждевременную универсализацию. Если два похожих участка пока развиваются по-разному, оставь их явными и понятными.

### SOLID

- Один класс — одна ответственность.
- Роуты FastAPI не должны содержать бизнес-логику.
- Сервисы не должны знать детали HTTP.
- Репозитории отвечают только за доступ к БД.
- Модули подключаются через `BaseModule` и `ModuleRegistry`.

## Правила структуры

### `__init__.py`

Файлы `__init__.py` должны быть пустыми или содержать только минимальную package-инициализацию без реэкспортов, сайд-эффектов и бизнес-логики.

Не делай так:

```python
from .service import SomeService
__all__ = ["SomeService"]
```

Импортируй напрямую из конкретного файла:

```python
from modules.web.services.vpn_service import VPNService
```

### Web-модуль

Ожидаемая структура:

```text
modules/web/
  app.py
  module.py
  models/
    base.py
    schemas.py
  repositories/
    admin_repo.py
    client_repo.py
    log_repo.py
    server_repo.py
    vpn_repo.py
  services/
    admin_service.py
    client_service.py
    log_service.py
    process_service.py
    stats_service.py
    vpn_service.py
  static/
  template/
```

Правила:

- `app.py` создаёт FastAPI-приложение и HTTP endpoints.
- `module.py` содержит `WebModule` и интеграцию с `BaseModule`.
- `services/*` содержат бизнес-логику.
- `repositories/*` содержат SQLAlchemy-запросы.
- `models/base.py` содержит SQLAlchemy ORM-модели web-модуля.
- `models/schemas.py` содержит Pydantic-схемы.

### Deploy-документация

Файлы инфраструктурных памяток и шаблонов лежат в `docs/deploy/`.

Текущая схема внешнего доступа:

```text
Cloudflare -> remote nginx:80/443 -> WireGuard tunnel -> local FastAPI:8000
```

Ключевые файлы:

- `docs/deploy/nginx-wireguard.md` — основная инструкция деплоя nginx + WireGuard.
- `docs/deploy/nginx-hromatite.conf` — шаблон nginx reverse proxy на локальный FastAPI через WireGuard.
- `docs/deploy/wireguard-server.conf.example` — пример WireGuard-конфига удалённого сервера.
- `docs/deploy/wireguard-client-windows.conf.example` — пример WireGuard-конфига локального Windows ПК.
- `docs/deploy/ssl-cloudflare.md` — памятка по включению SSL через Cloudflare.

Если меняешь порты, сетевую схему, reverse proxy, способ запуска web-модуля или требования Cloudflare/WireGuard, обновляй соответствующие файлы в `docs/deploy/`.

## Импорты

Используй абсолютные импорты от корня проекта:

```python
from modules.web.repositories.vpn_repo import VPNRepo
from modules.web.models.base import VPNConfig
```

Не используй несуществующий пакет `web`:

```python
from web.models.base import VPNConfig
```

## Работа с БД и миграциями

Проект использует Alembic. Не полагайся на `Base.metadata.create_all()` в runtime.

Если добавляешь или меняешь SQLAlchemy-модель:

1. Проверь `modules/web/models/base.py`.
2. Проверь `models/models.py`, если Alembic должен видеть новую модель.
3. Добавь новую миграцию в `migrations/versions/`.
4. Укажи корректный `down_revision`.
5. Для SQLite используй `op.batch_alter_table()` при изменении существующих таблиц.
6. Миграция должна быть идемпотентной, если проект уже мог иметь частично созданную схему.

Минимальный шаблон проверки существования таблицы:

```python
def _has_table(table_name: str) -> bool:
    bind = op.get_bind()
    inspector = sa.inspect(bind)
    return table_name in inspector.get_table_names()
```

Минимальный шаблон проверки колонки:

```python
def _has_column(table_name: str, column_name: str) -> bool:
    bind = op.get_bind()
    inspector = sa.inspect(bind)
    if table_name not in inspector.get_table_names():
        return False
    return column_name in {column["name"] for column in inspector.get_columns(table_name)}
```

### Важный инвариант Alembic

`migrations/env.py` импортирует:

```python
from models.models import Base
```

Поэтому `models/models.py` обязан экспортировать `Base` без circular import.

Правильная идея:

```python
from modules.web.models.base import Base
```

Нельзя делать:

```python
from .models import Base
```

## Модульная регистрация

`main.py` содержит `AVAILABLE_MODULES`. Если добавляешь модуль, он должен:

- наследоваться от `core.module.BaseModule`;
- иметь поле `name`;
- реализовать `setup`, `run`, `teardown` при необходимости;
- быть добавлен в `AVAILABLE_MODULES`.

Если модуль указан в `.env` в `ENABLED_MODULES`, но не зарегистрирован, приложение выведет warning `Unknown module`.

## FastAPI endpoints

Роут должен быть тонким:

```python
@app.get("/api/configs")
async def configs(session: AsyncSession = Depends(get_session)):
    return await VPNService(session).list_configs()
```

Не размещай SQLAlchemy-запросы прямо в endpoint.

Ошибки домена перехватывай и переводь в HTTPException:

```python
try:
    return await service.create(...)
except ValueError as error:
    raise HTTPException(status_code=400, detail=str(error)) from error
```

## UI админки

Админка хранится в:

- `modules/web/template/admin.html`
- `modules/web/template/auth.html`
- `modules/web/template/partials/*.html`
- `modules/web/static/admin.css`
- `modules/web/static/admin.js`
- `modules/web/static/auth.css`
- `modules/web/static/auth.js`

Правила:

- UI должен работать без сборщика фронтенда.
- Не добавляй React/Vue/Vite без явного запроса.
- API-вызовы должны быть централизованы в маленькой функции `api()`.
- Интерфейс должен оставаться модульным: отдельные функции загрузки для stats/configs/servers/clients/admins/logs/process/health.
- Основной `admin.html` должен оставаться оболочкой панели; содержимое вкладок хранится в отдельных partial-файлах в `modules/web/template/partials/`.
- Вкладки админки загружаются частями через `GET /admin/partials/{partial_name}`. Новые partial endpoints добавляй только через whitelist в `modules/web/app.py`.
- Отдельные страницы авторизации пользователей доступны на `GET /login` и `GET /register`; они используют `auth.html`, `auth.css` и `auth.js`, не встраивай форму логина в `admin.html`.
- Auth API: `POST /api/auth/login` выдаёт JWT с `role`, `POST /api/auth/register` создаёт обычного пользователя с ролью `user`.
- Админские API защищены `Authorization: Bearer <token>` и доступны только ролям `admin`, `superadmin`, `owner`; обычная роль `user` не должна получать доступ к `/api/stats`, `/api/configs`, `/api/servers`, `/api/clients`, `/api/logs`, `/api/process`, `/api/admins`.
- В web-модуле есть in-memory rate limit по IP и временный ban для частых неудачных логинов. Настройки находятся в `core.config.WebConfig`: `rate_limit_requests`, `rate_limit_window_seconds`, `login_max_attempts`, `login_ban_seconds`, env-переменные — `WEB_RATE_LIMIT_REQUESTS`, `WEB_RATE_LIMIT_WINDOW_SECONDS`, `WEB_LOGIN_MAX_ATTEMPTS`, `WEB_LOGIN_BAN_SECONDS`.
- При любых изменениях структуры админки, partial-шаблонов, JS/CSS панели или связанных web endpoints обновляй этот раздел `README_AI.md`.

## Мониторинг и серверы

- Управление VPN-серверами реализовано через `VPNServer`, `ServerRepo` и `ServerService`.
- HTTP endpoints серверов находятся в `modules/web/app.py` под `/api/servers`.
- Проверка состояния сервера сейчас TCP-check по `host:ssh_port`; не добавляй SSH-команды без явного запроса.
- Prometheus-compatible метрики доступны на `GET /metrics` без отдельной зависимости `prometheus-client`.
- Если добавляешь новые счётчики/статусы, обновляй одновременно `StatsService`, `/api/health`, `/metrics` и UI админки.

## Pytest

Тесты находятся в:

- `tests/conftest.py`
- `tests/test_web_api.py`
- `tests/test_web_services.py`

Правила:

- Используй `pytest` и `pytest-asyncio`.
- Для web/API тестов используй `httpx.ASGITransport`, не поднимай реальный uvicorn-сервер.
- Для БД в тестах используй временную SQLite БД из `tmp_path`.
- Таблицы в тестах можно создавать через `Base.metadata.create_all`, runtime проекта всё равно должен использовать Alembic.
- Внешние проверки сети, SSH и контейнеров мокай через `monkeypatch`.
- Запускай тесты через `uv run pytest`, если зависимости установлены в `.venv` через uv.

## Стиль кода

- Python: явные имена, короткие функции, минимум магии.
- Не добавляй комментарии ради комментариев.
- Не меняй формат всего файла без необходимости.
- Не смешивай разные уровни ответственности в одном файле.
- Не используй wildcard imports.
- Не добавляй сайд-эффекты на import.

## Проверки после изменений

Минимально выполни:

```bash
python -m compileall main.py run_web.py core modules models migrations tests
```

Если зависимости доступны только через venv/uv, используй команду запуска проекта из IDE или локальный интерпретатор `.venv`.

Проверь импорт Alembic Base:

```bash
python -c "from models.models import Base; print(sorted(Base.metadata.tables.keys()))"
```

Запусти тесты:

```bash
uv run pytest
```

Проверь web-админку:

```text
http://127.0.0.1:8000/admin
```

Проверь API:

```text
GET /api/health
GET /api/stats
GET /api/configs
GET /api/servers
GET /api/clients
GET /api/logs
GET /metrics
```

## Типовые ошибки и решения

### Circular import в `models.models`

Симптом:

```text
ImportError: cannot import name 'BaseModel' from partially initialized module 'models.models'
```

Причина: файл `models/models.py` импортирует сам себя.

Решение: импортировать реальные модели из `modules.web.models.base`.

### `no such table: client_devices`

Симптом:

```text
sqlite3.OperationalError: no such table: client_devices
```

Причина: модель есть, но Alembic-миграция не создала таблицу.

Решение: добавить миграцию, создающую `client_devices`, а также другие недостающие таблицы web-админки.

### `Unknown module: bot` или `Unknown module: vpn`

Симптом:

```text
Unknown module: bot (skipped)
Unknown module: vpn (skipped)
```

Причина: модуль указан в `ENABLED_MODULES`, но отсутствует в `AVAILABLE_MODULES`.

Решение: либо реализовать и зарегистрировать модуль, либо убрать его из `.env`.

### 404 на `/`

Это нормально, если корневой endpoint не реализован. Админка находится на:

```text
/admin
```

Если нужен redirect, добавь в `modules/web/app.py`:

```python
@app.get("/")
async def root():
    return RedirectResponse("/admin")
```

## Безопасность

- Не хардкодь токены, пароли и API ключи.
- Используй `.env` и `core.config`.
- Не логируй секреты.
- Админские endpoints должны быть подготовлены к авторизации, если проект выходит за пределы локальной разработки.

## Перед изменениями всегда

1. Найди актуальный файл, не полагайся на память.
2. Проверь импорты и call sites.
3. Делай маленькие изменения.
4. После изменений запускай compile/import checks.
5. Если менял модели — добавляй миграцию.
6. Не трогай чужие несвязанные изменения.
