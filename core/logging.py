import logging
import sys
from pathlib import Path

Logger = logging.Logger
LogRecord = logging.LogRecord

DEBUG = logging.DEBUG
INFO = logging.INFO
WARNING = logging.WARNING
ERROR = logging.ERROR
CRITICAL = logging.CRITICAL

__all__ = (
    "setup_logging",
    "getLogger",
    "Logger",
    "LogRecord",
    "DEBUG",
    "INFO",
    "WARNING",
    "ERROR",
    "CRITICAL",
)


class _ColorFormatter(logging.Formatter):
    _COLORS = {
        logging.DEBUG: "\033[36m",     # cyan
        logging.INFO: "\033[32m",      # green
        logging.WARNING: "\033[33m",   # yellow
        logging.ERROR: "\033[31m",     # red
        logging.CRITICAL: "\033[1;31m" # bold red
    }
    _RESET = "\033[0m"
    _GRAY = "\033[90m"

    def format(self, record: logging.LogRecord) -> str:
        color = self._COLORS.get(record.levelno, "")
        # Colorize the level name
        record.levelname = f"{color}{record.levelname:<8}{self._RESET}"
        # Gray out the module path
        record.name = f"{self._GRAY}{record.name}{self._RESET}"
        return super().format(record)


# Loggers to suppress in console output
_NOISY_LOGGERS = (
    "aiogram.event",
    "aiogram.dispatcher",
    "aiogram.session",
    "dishka",
    "aiosqlite",
    "httpx",
    "httpcore",
    "aiohttp.access",
    "aiohttp.client",
    "sqlalchemy.engine",
    "sqlalchemy.pool",
)


class _ConsoleFilter(logging.Filter):
    """Filter out noisy third-party logs from console output."""

    def filter(self, record: logging.LogRecord) -> bool:
        return not any(record.name.startswith(name) for name in _NOISY_LOGGERS)


def setup_logging(level: int = logging.INFO, log_file: Path = Path("logs/bot.log")) -> None:
    log_file.parent.mkdir(parents=True, exist_ok=True)

    root = logging.getLogger()
    root.setLevel(logging.DEBUG)  # Capture all levels
    root.handlers.clear()

    # Console handler: filtered + colored (INFO and above for app logs)
    console_handler = logging.StreamHandler(sys.stdout)
    console_handler.setLevel(level)
    console_handler.addFilter(_ConsoleFilter())
    console_handler.setFormatter(_ColorFormatter(
        fmt="%(asctime)s │ %(levelname)s │ %(name)s │ %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    ))
    root.addHandler(console_handler)

    # File handler: all logs unfiltered (overwrite on each run)
    file_handler = logging.FileHandler(log_file, mode="w", encoding="utf-8")
    file_handler.setLevel(logging.DEBUG)
    file_handler.setFormatter(logging.Formatter(
        fmt="%(asctime)s │ %(levelname)s │ %(name)s │ %(message)s",
        datefmt="%Y-%m-%d %H:%M:%S",
    ))
    root.addHandler(file_handler)

    # Quieten noisy third-party loggers in console only (file still gets them)
    for name in _NOISY_LOGGERS:
        logging.getLogger(name).setLevel(logging.WARNING)


def getLogger(name: str | None = None) -> logging.Logger:
    """Return a logger that goes through the configured logging stack."""
    return logging.getLogger(name)
