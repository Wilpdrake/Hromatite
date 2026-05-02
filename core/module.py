from abc import ABC, abstractmethod
from typing import Any

from dishka import Provider


class BaseModule(ABC):
    """Базовый класс для всех модулей приложения."""

    name: str

    def get_providers(self) -> list[Provider]:
        """Возвращает DI-провайдеры модуля."""
        return []

    async def setup(self) -> None:
        """Инициализация модуля (вызывается один раз при старте)."""
        pass

    async def run(self) -> None:
        """Запуск модуля (основной цикл или сервер)."""
        pass

    async def teardown(self) -> None:
        """Очистка ресурсов при завершении модуля."""
        pass
