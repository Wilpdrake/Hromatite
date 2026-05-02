"""
Standalone script to run the web admin panel
Usage: python run_web.py
"""
import asyncio
from dotenv import load_dotenv

from core.config import load_config
from core.database import init_db
from core.di import AppProvider
from core.logging import setup_logging, getLogger, DEBUG
from modules.web.module import WebModule

logger = getLogger(__name__)


async def main():
    setup_logging(level=DEBUG)
    logger.info("Starting Web Admin Panel...")
    
    # Load environment
    load_dotenv()
    
    # Load config
    config = load_config()
    logger.info("Config loaded")
    
    # Initialize database
    await init_db(config.database_url)
    logger.info("Database initialized")
    
    # Create and run web module
    web_module = WebModule(config)
    await web_module.setup()
    
    logger.info("Web admin available at: http://%s:%s/admin", config.web.host, config.web.port)
    
    try:
        await web_module.run()
    except KeyboardInterrupt:
        logger.info("Received shutdown signal")
    finally:
        await web_module.teardown()
        logger.info("Web server stopped")


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
