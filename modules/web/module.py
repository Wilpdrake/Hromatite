import uvicorn

from core.config import Config
from core.logging import getLogger
from core.module import BaseModule
from modules.web.app import create_app

logger = getLogger(__name__)


class WebModule(BaseModule):
    name = "web"

    def __init__(self, config: Config):
        self.config = config
        self.app = create_app(config)
        self.server: uvicorn.Server | None = None

    async def setup(self) -> None:
        logger.info("Web module configured on %s:%s", self.config.web.host, self.config.web.port)

    async def run(self) -> None:
        server_config = uvicorn.Config(
            self.app,
            host=self.config.web.host,
            port=self.config.web.port,
            log_level="info",
        )
        self.server = uvicorn.Server(server_config)
        await self.server.serve()

    async def teardown(self) -> None:
        if self.server:
            self.server.should_exit = True
        logger.info("Web module stopped")
