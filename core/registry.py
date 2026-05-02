import asyncio
from typing import Any

from dishka import Provider

from .module import BaseModule
from core.logging import getLogger

logger = getLogger(__name__)


class ModuleRegistry:
    """Реестр модулей: регистрация, инициализация, запуск, остановка."""

    def __init__(self) -> None:
        self._modules: dict[str, BaseModule] = {}

    def register(self, module: BaseModule) -> None:
        if module.name in self._modules:
            raise ValueError(f"Module '{module.name}' is already registered")
        self._modules[module.name] = module
        logger.info("Module registered: %s", module.name)

    def get_all_providers(self) -> list[Provider]:
        """Собирает DI-провайдеры из всех зарегистрированных модулей."""
        providers: list[Provider] = []
        for module in self._modules.values():
            providers.extend(module.get_providers())
        return providers

    async def setup_all(self) -> None:
        """Инициализирует все модули."""
        for module in self._modules.values():
            logger.info("Setting up module: %s", module.name)
            await module.setup()

    async def run_all(self) -> None:
        """Запускает все модули параллельно и ждёт завершения."""
        tasks = []
        for module in self._modules.values():
            logger.info("Starting module: %s", module.name)
            tasks.append(asyncio.create_task(module.run(), name=module.name))
        await asyncio.gather(*tasks)

    async def teardown_all(self) -> None:
        """Останавливает все модули."""
        for module in self._modules.values():
            logger.info("Tearing down module: %s", module.name)
            await module.teardown()

    @property
    def modules(self) -> dict[str, BaseModule]:
        return self._modules
