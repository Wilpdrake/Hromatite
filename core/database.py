import asyncio
from pathlib import Path

from alembic import command
from alembic.config import Config as AlembicConfig
from sqlalchemy.engine.url import make_url
from sqlalchemy.ext.asyncio import create_async_engine, async_sessionmaker, AsyncSession

from core.logging import getLogger

logger = getLogger(__name__)

def create_session_maker(db_url: str = "sqlite+aiosqlite:///bot.db") -> async_sessionmaker[AsyncSession]:
    engine = create_async_engine(db_url, echo=False)
    logger.info("Session maker created for %s", db_url)
    return async_sessionmaker(engine, expire_on_commit=False, class_=AsyncSession)

def _sync_driver_url(db_url: str) -> str:
    url = make_url(db_url)
    if "+" in url.drivername:
        backend, _driver = url.drivername.split("+", 1)
        url = url.set(drivername=backend)
    return url.render_as_string(hide_password=False)


async def init_db(db_url: str = "sqlite+aiosqlite:///bot.db"):
    logger.info("Initializing database (via migrations): %s", db_url)

    def run_migrations():
        config_path = Path(__file__).resolve().parent.parent / "alembic.ini"
        alembic_config = AlembicConfig(str(config_path))
        alembic_config.set_main_option("sqlalchemy.url", _sync_driver_url(db_url))
        alembic_config.attributes["configure_logger"] = False
        command.upgrade(alembic_config, "head")

    try:
        await asyncio.to_thread(run_migrations)
    except Exception:  # pragma: no cover - logging branch
        logger.exception("Database migration failed")
        raise
    else:
        logger.info("Database is up to date")