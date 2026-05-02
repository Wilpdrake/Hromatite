from typing import AsyncIterable
from dishka import Provider, Scope, provide
from sqlalchemy.ext.asyncio import async_sessionmaker, AsyncSession

from core.config import Config, load_config
from core.database import create_session_maker
from core.logging import getLogger

logger = getLogger(__name__)


class AppProvider(Provider):
    # 1. Глобальные настройки (Scope.APP)
    @provide(scope=Scope.APP)
    def get_config(self) -> Config:
        config = load_config()
        logger.debug("Config loaded, enabled modules: %s", config.enabled_modules)
        return config

    @provide(scope=Scope.APP)
    def get_session_maker(self, config: Config) -> async_sessionmaker[AsyncSession]:
        session_maker = create_session_maker(config.database_url)
        logger.debug("Session maker provided")
        return session_maker

    # 2. Подключения на каждый запрос (Scope.REQUEST)
    @provide(scope=Scope.REQUEST)
    async def get_session(self, session_maker: async_sessionmaker[AsyncSession]) -> AsyncIterable[AsyncSession]:
        async with session_maker() as session:
            yield session