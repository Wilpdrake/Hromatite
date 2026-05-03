import asyncio
import os
import shlex
import textwrap
from dataclasses import dataclass

import asyncssh
from dotenv import load_dotenv


@dataclass(frozen=True)
class DeployConfig:
    host: str
    user: str
    port: int
    password: str | None
    key_path: str | None
    use_sudo: bool
    domain: str
    wg_port: int
    wg_server_private_key: str
    wg_client_public_key: str
    wg_server_address: str
    wg_client_address: str
    fastapi_port: int
    install_packages: bool
    reload_services: bool


def env_bool(name: str, default: bool) -> bool:
    value = os.getenv(name)
    if value is None:
        return default
    return value.strip().lower() in {"1", "true", "yes", "y", "on"}


def require_env(name: str) -> str:
    value = os.getenv(name, "").strip()
    if not value:
        raise RuntimeError(f"Missing required environment variable: {name}")
    return value


def load_deploy_config() -> DeployConfig:
    load_dotenv()
    return DeployConfig(
        host=require_env("VPN_SERVER_HOST"),
        user=require_env("VPN_SERVER_USER"),
        port=int(os.getenv("VPN_SERVER_PORT", "22")),
        password=os.getenv("VPN_SERVER_PASSWORD") or None,
        key_path=os.getenv("VPN_SERVER_SSH_KEY") or None,
        use_sudo=env_bool("USE_SUDO", True),
        domain=require_env("DEPLOY_DOMAIN"),
        wg_port=int(os.getenv("WG_PORT", "51820")),
        wg_server_private_key=require_env("WG_SERVER_PRIVATE_KEY"),
        wg_client_public_key=require_env("WG_CLIENT_PUBLIC_KEY"),
        wg_server_address=os.getenv("WG_SERVER_ADDRESS", "10.66.66.1/24"),
        wg_client_address=os.getenv("WG_CLIENT_ADDRESS", "10.66.66.2"),
        fastapi_port=int(os.getenv("DEPLOY_FASTAPI_PORT", os.getenv("WEB_PORT", "8000"))),
        install_packages=env_bool("DEPLOY_INSTALL_PACKAGES", True),
        reload_services=env_bool("DEPLOY_RELOAD_SERVICES", True),
    )


def nginx_config(config: DeployConfig) -> str:
    upstream = f"http://{config.wg_client_address}:{config.fastapi_port}"
    return textwrap.dedent(
        f"""
        server {{
            listen 80;
            server_name {config.domain};

            client_max_body_size 20m;

            location / {{
                proxy_pass {upstream};
                proxy_http_version 1.1;
                proxy_set_header Host $host;
                proxy_set_header X-Real-IP $remote_addr;
                proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
                proxy_set_header X-Forwarded-Proto $scheme;
                proxy_set_header Upgrade $http_upgrade;
                proxy_set_header Connection "upgrade";
                proxy_read_timeout 300s;
                proxy_connect_timeout 30s;
                proxy_send_timeout 300s;
            }}
        }}

        server {{
            listen 443;
            server_name {config.domain};

            return 444;
        }}
        """
    ).strip() + "\n"


def wireguard_config(config: DeployConfig) -> str:
    return textwrap.dedent(
        f"""
        [Interface]
        Address = {config.wg_server_address}
        ListenPort = {config.wg_port}
        PrivateKey = {config.wg_server_private_key}

        [Peer]
        PublicKey = {config.wg_client_public_key}
        AllowedIPs = {config.wg_client_address}/32
        PersistentKeepalive = 25
        """
    ).strip() + "\n"


class RemoteGatewayDeployer:
    def __init__(self, config: DeployConfig):
        self.config = config
        self.conn: asyncssh.SSHClientConnection | None = None

    async def run(self) -> None:
        client_keys = [self.config.key_path] if self.config.key_path else None
        async with asyncssh.connect(
            self.config.host,
            username=self.config.user,
            port=self.config.port,
            password=self.config.password,
            client_keys=client_keys,
            known_hosts=None,
        ) as conn:
            self.conn = conn
            if self.config.install_packages:
                await self.install_packages()
            await self.require_command("nginx")
            await self.require_command("wg")
            await self.exec("mkdir -p /etc/wireguard /etc/nginx/sites-available /etc/nginx/sites-enabled", sudo=True)
            await self.write_remote_file("/etc/wireguard/wg0.conf", wireguard_config(self.config), mode="600")
            await self.write_remote_file("/etc/nginx/sites-available/hromatite.conf", nginx_config(self.config), mode="644")
            await self.exec("ln -sfn /etc/nginx/sites-available/hromatite.conf /etc/nginx/sites-enabled/hromatite.conf", sudo=True)
            await self.exec("printf '%s\n' 'net.ipv4.ip_forward=1' > /etc/sysctl.d/99-hromatite-wireguard.conf", sudo=True)
            await self.exec("sysctl --system", sudo=True)
            await self.open_firewall_port()
            await self.exec("nginx -t", sudo=True)
            if self.config.reload_services:
                await self.reload_services()
            await self.exec("wg show", sudo=True, check=False)

    async def install_packages(self) -> None:
        if await self.command_exists("apt-get"):
            await self.exec("apt-get update && apt-get install -y nginx wireguard", sudo=True)
            return
        if await self.command_exists("dnf"):
            await self.exec("dnf install -y nginx wireguard-tools", sudo=True)
            return
        if await self.command_exists("yum"):
            await self.exec("yum install -y nginx wireguard-tools", sudo=True)
            return
        if await self.command_exists("pacman"):
            await self.exec("pacman -Sy --noconfirm nginx wireguard-tools", sudo=True)
            return
        raise RuntimeError("No supported package manager found. Install nginx and WireGuard manually, or set DEPLOY_INSTALL_PACKAGES=false.")

    async def open_firewall_port(self) -> None:
        if await self.command_exists("ufw"):
            await self.exec(f"ufw allow {self.config.wg_port}/udp", sudo=True, check=False)
            return
        if await self.command_exists("firewall-cmd"):
            await self.exec(f"firewall-cmd --permanent --add-port={self.config.wg_port}/udp && firewall-cmd --reload", sudo=True, check=False)
            return
        print("No ufw/firewalld found; skipping firewall configuration")

    async def reload_services(self) -> None:
        if await self.command_exists("systemctl"):
            await self.exec("systemctl enable --now wg-quick@wg0", sudo=True)
            await self.exec("systemctl reload nginx || systemctl restart nginx", sudo=True)
            return
        if await self.command_exists("service"):
            await self.exec("wg-quick up wg0 || true", sudo=True)
            await self.exec("service nginx reload || service nginx restart", sudo=True)
            return
        raise RuntimeError("No systemctl/service found. Start wg-quick@wg0 and reload nginx manually, or set DEPLOY_RELOAD_SERVICES=false.")

    async def require_command(self, command: str) -> None:
        if not await self.command_exists(command):
            raise RuntimeError(f"Required command not found on remote server: {command}")

    async def command_exists(self, command: str) -> bool:
        if self.conn is None:
            raise RuntimeError("SSH connection is not initialized")
        result = await self.conn.run(f"command -v {shlex.quote(command)} >/dev/null 2>&1", check=False)
        return result.exit_status == 0

    async def exec(self, command: str, sudo: bool = False, check: bool = True):
        if self.conn is None:
            raise RuntimeError("SSH connection is not initialized")
        remote_command = self.sudo(command) if sudo else command
        print(f"$ {remote_command}")
        result = await self.conn.run(remote_command, check=False)
        if result.stdout:
            print(result.stdout.rstrip())
        if result.stderr:
            print(result.stderr.rstrip())
        if check and result.exit_status != 0:
            raise RuntimeError(f"Remote command failed with exit status {result.exit_status}: {command}")
        return result

    async def write_remote_file(self, path: str, content: str, mode: str) -> None:
        quoted_path = shlex.quote(path)
        await self.exec(f"cat > {quoted_path} <<'EOF'\n{content}EOF", sudo=True)
        await self.exec(f"chmod {shlex.quote(mode)} {quoted_path}", sudo=True)

    def sudo(self, command: str) -> str:
        if not self.config.use_sudo:
            return command
        return f"sudo sh -c {shlex.quote(command)}"


async def main() -> None:
    config = load_deploy_config()
    await RemoteGatewayDeployer(config).run()
    print("Remote nginx/WireGuard gateway deploy completed")


if __name__ == "__main__":
    asyncio.run(main())
