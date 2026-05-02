import asyncio

from dotenv import load_dotenv
from dishka import make_async_container

from core.config import load_config
from core.database import init_db
from core.di import AppProvider
from core.logging import setup_logging, getLogger, DEBUG
from core.registry import ModuleRegistry

from modules.web.module import WebModule

logger = getLogger(__name__)

AVAILABLE_MODULES = {
    "web": WebModule,
}


async def main():
    setup_logging(level=DEBUG)
    logger.info("Starting Hromatite...")

    # 1. Загрузка .env
    load_dotenv()

    # 2. Загрузка конфигурации
    config = load_config()
    logger.info("Config loaded, enabled modules: %s", config.enabled_modules)

    # 3. Инициализация БД
    await init_db(config.database_url)

    # 4. Регистрация модулей
    registry = ModuleRegistry()
    for module_name in config.enabled_modules:
        module_cls = AVAILABLE_MODULES.get(module_name)
        if module_cls is None:
            logger.warning("Unknown module: %s (skipped)", module_name)
            continue
        registry.register(module_cls(config))

    # 5. Настройка DI (Dishka) — общий контейнер для всех модулей
    container = make_async_container(AppProvider(), *registry.get_all_providers())

    # 6. Инициализация всех модулей
    await registry.setup_all()
    logger.info("All modules initialized")

    # Для бот-модуля: подключаем dishka к aiogram (после setup, т.к. dp создаётся там)
    if "bot" in registry.modules:
        from dishka.integrations.aiogram import setup_dishka
        bot_module = registry.modules["bot"]
        setup_dishka(container=container, router=bot_module.dp)

    # 7. Запуск всех модулей параллельно
    try:
        logger.info("Starting all modules...")
        await registry.run_all()
    finally:
        logger.info("Shutting down...")
        await registry.teardown_all()
        logger.info("Shutdown complete")


if __name__ == "__main__":
    asyncio.run(main())