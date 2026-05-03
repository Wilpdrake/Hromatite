from modules.web.models.schemas import WGConfig, WGPeerConfig, WGServerConfig
from modules.web.repositories.client_repo import ClientRepo


class ClientService:
    def __init__(self, session):
        self.session = session

    async def list_clients(self, telegram_id=None, skip=0, limit=100):
        repo = ClientRepo(self.session)
        items, total = await repo.find_all(telegram_id=telegram_id, skip=skip, limit=limit)
        return {"items": [self._to_dict(item) for item in items], "total": total}

    async def create_client(self, device_name, telegram_id=None, allowed_ips="0.0.0.0/0", enabled=True):
        repo = ClientRepo(self.session)
        return await repo.create(device_name, telegram_id, allowed_ips, enabled)

    async def update_client(self, cid, **kwargs):
        repo = ClientRepo(self.session)
        return await repo.update(cid, **kwargs)

    async def delete_client(self, cid):
        repo = ClientRepo(self.session)
        return await repo.delete(cid)

    async def generate_client_config(self, client, primary_server):
        peer = WGPeerConfig(
            private_key=client.private_key,
            public_key=primary_server.pub_key if hasattr(primary_server, "pub_key") else "",
            preshared_key=client.preshared_key or "",
            endpoint=primary_server.host or "",
            allowed_ips=client.allowed_ips,
            persistent_keepalive=25,
        )
        interface = WGConfig(
            private_key=client.private_key,
            address="10.8.0.0/24",
            dns=["8.8.8.8"],
        )
        return WGServerConfig(interface=interface, server={"listen_port": 51820}, peers=[peer])

    @staticmethod
    def _to_dict(item):
        return {
            "id": item.id,
            "device_name": item.device_name,
            "public_key": item.public_key,
            "allowed_ips": item.allowed_ips,
            "enabled": item.enabled,
            "last_handshake": str(item.last_handshake) if item.last_handshake else None,
            "bytes_in": item.bytes_in,
            "bytes_out": item.bytes_out,
        }
