import argparse
import os
import sys
from pathlib import Path

from alembic import command
from alembic.config import Config as AlembicConfig
from dotenv import load_dotenv
from sqlalchemy.engine.url import make_url

PROJECT_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(PROJECT_ROOT))


def sync_driver_url(db_url: str) -> str:
    url = make_url(db_url)
    if "+" in url.drivername:
        backend, _driver = url.drivername.split("+", 1)
        url = url.set(drivername=backend)
    return url.render_as_string(hide_password=False)


def build_alembic_config(db_url: str | None) -> AlembicConfig:
    config = AlembicConfig(str(PROJECT_ROOT / "alembic.ini"))
    config.attributes["configure_logger"] = False
    if db_url:
        config.set_main_option("sqlalchemy.url", sync_driver_url(db_url))
    return config


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Create Alembic autogenerate revision and upgrade database to it."
    )
    parser.add_argument(
        "message",
        nargs="?",
        default="auto migration",
        help="Migration message used for the generated revision filename.",
    )
    parser.add_argument(
        "--database-url",
        default=None,
        help="Database URL. Defaults to DATABASE_URL from environment or alembic.ini.",
    )
    return parser.parse_args()


def main() -> None:
    load_dotenv(PROJECT_ROOT / ".env")
    args = parse_args()
    db_url = args.database_url or os.getenv("DATABASE_URL")
    config = build_alembic_config(db_url)

    revision = command.revision(config, message=args.message, autogenerate=True)
    if revision is None:
        raise RuntimeError("Alembic did not create a revision")

    command.upgrade(config, "head")
    print(f"Created and applied migration: {revision.revision}")


if __name__ == "__main__":
    main()
