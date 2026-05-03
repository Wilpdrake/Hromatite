import base64
import os

from modules.web.repositories.vpn_repo import VPNRepo


class ProcessService:
    def __init__(self, session):
        self.session = session

    async def get_status(self):
        repo = VPNRepo(self.session)
        active = len(await repo.get_enabled())
        return {
            "wg_running": True,
            "xray_running": True,
            "wg_clients_online": 0,
            "wg_clients_total": active,
            "wg_bytes_in": 0,
            "wg_bytes_out": 0,
        }

    async def reload_wg(self):
        return {"status": "ok", "action": "wg reload"}

    async def reload_xray(self):
        return {"status": "ok", "action": "xray reload"}

    async def get_vpn_config_gen(self):
        private_key = base64.b64encode(os.urandom(32)).decode()
        public_key = base64.b64encode(os.urandom(32)).decode()
        return {
            "interface": {"private_key": private_key, "address": "10.8.0.0/24", "listen_port": 51820},
            "server": {"publicKey": public_key, "port": 51820},
            "dns": ["8.8.8.8"],
        }
