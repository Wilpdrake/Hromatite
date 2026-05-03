import os
from dataclasses import dataclass, field


@dataclass
class BotConfig:
    token: str = ""
    admin_id: int = 0
    price_per_device: int = 100
    cryptopay_token: str = ""
    cryptopay_asset: str = ""


@dataclass
class VPNConfig:
    server_host: str = ""
    server_user: str = ""
    server_password: str = ""
    server_port: int = 22
    xray_port: int = 443
    awg_port: int = 55424
    reality_server_name: str = "www.googletagmanager.com"
    xray_container: str = "hromatite-xray"
    awg_container: str = "hromatite-awg"
    use_sudo: bool = True


@dataclass
class WebConfig:
    host: str = "0.0.0.0"
    port: int = 8000
    admin_token: str = ""
    rate_limit_requests: int = 120
    rate_limit_window_seconds: int = 60
    login_max_attempts: int = 5
    login_ban_seconds: int = 900


@dataclass
class PterodactylConfig:
    panel_url: str = ""
    api_key: str = ""


@dataclass
class Config:
    enabled_modules: list[str] = field(default_factory=lambda: ["bot"])
    database_url: str = "sqlite+aiosqlite:///bot.db"
    bot: BotConfig = field(default_factory=BotConfig)
    vpn: VPNConfig = field(default_factory=VPNConfig)
    web: WebConfig = field(default_factory=WebConfig)
    pterodactyl: PterodactylConfig = field(default_factory=PterodactylConfig)


def load_config() -> Config:
    enabled = os.getenv("ENABLED_MODULES", "bot")
    return Config(
        enabled_modules=[m.strip() for m in enabled.split(",")],
        database_url=os.getenv("DATABASE_URL", "sqlite+aiosqlite:///bot.db"),
        bot=BotConfig(
            token=os.getenv("BOT_TOKEN", ""),
            admin_id=int(os.getenv("ADMIN_ID", "0")),
            price_per_device=int(os.getenv("PRICE_PER_DEVICE", "100")),
            cryptopay_token=os.getenv("CRYPTO_PAY_TOKEN", ""),
            cryptopay_asset=os.getenv("CRYPTO_PAY_ASSET", ""),
        ),
        vpn=VPNConfig(
            server_host=os.getenv("VPN_SERVER_HOST", ""),
            server_user=os.getenv("VPN_SERVER_USER", ""),
            server_password=os.getenv("VPN_SERVER_PASSWORD", ""),
            server_port=int(os.getenv("VPN_SERVER_PORT", "22")),
            xray_port=int(os.getenv("XRAY_PORT", "443")),
            awg_port=int(os.getenv("AWG_PORT", "55424")),
            reality_server_name=os.getenv("XRAY_REALITY_SERVER_NAME", "www.googletagmanager.com"),
            xray_container=os.getenv("XRAY_CONTAINER", "hromatite-xray"),
            awg_container=os.getenv("AWG_CONTAINER", "hromatite-awg"),
            use_sudo=os.getenv("USE_SUDO", "true").lower() in ("true", "1", "yes"),
        ),
        web=WebConfig(
            host=os.getenv("WEB_HOST", "0.0.0.0"),
            port=int(os.getenv("WEB_PORT", "8000")),
            admin_token=os.getenv("WEB_ADMIN_TOKEN", ""),
            rate_limit_requests=int(os.getenv("WEB_RATE_LIMIT_REQUESTS", "120")),
            rate_limit_window_seconds=int(os.getenv("WEB_RATE_LIMIT_WINDOW_SECONDS", "60")),
            login_max_attempts=int(os.getenv("WEB_LOGIN_MAX_ATTEMPTS", "5")),
            login_ban_seconds=int(os.getenv("WEB_LOGIN_BAN_SECONDS", "900")),
        ),
        pterodactyl=PterodactylConfig(
            panel_url=os.getenv("PTERODACTYL_PANEL_URL", ""),
            api_key=os.getenv("PTERODACTYL_API_KEY", ""),
        ),
    )