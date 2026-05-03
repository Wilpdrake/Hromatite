from pathlib import Path
import sys

import pytest
from httpx import ASGITransport, AsyncClient
from sqlalchemy.ext.asyncio import async_sessionmaker, create_async_engine

ROOT_DIR = Path(__file__).resolve().parents[1]
if str(ROOT_DIR) not in sys.path:
    sys.path.insert(0, str(ROOT_DIR))

from core.config import Config, WebConfig
from modules.web.app import create_app
from modules.web.models.base import Base


@pytest.fixture()
def database_url(tmp_path: Path):
    return f"sqlite+aiosqlite:///{tmp_path / 'test.db'}"


@pytest.fixture()
async def session_maker(database_url):
    engine = create_async_engine(database_url, echo=False)
    async with engine.begin() as connection:
        await connection.run_sync(Base.metadata.create_all)
    maker = async_sessionmaker(engine, expire_on_commit=False)
    yield maker
    await engine.dispose()


@pytest.fixture()
async def session(session_maker):
    async with session_maker() as item:
        yield item


@pytest.fixture()
def test_config(database_url):
    return Config(
        enabled_modules=["web"],
        database_url=database_url,
        web=WebConfig(host="127.0.0.1", port=8000, admin_token="test-secret"),
    )


@pytest.fixture()
async def client(test_config, session_maker):
    app = create_app(test_config)
    transport = ASGITransport(app=app)
    async with AsyncClient(transport=transport, base_url="http://testserver") as item:
        yield item
