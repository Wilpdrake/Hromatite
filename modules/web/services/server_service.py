import asyncio

from modules.web.repositories.server_repo import ServerRepo


class ServerService:
    def __init__(self, session):
        self.session = session

    async def list_servers(self):
        repo = ServerRepo(self.session)
        servers = await repo.find_all()
        return {"items": [self._to_dict(item) for item in servers], "total": len(servers)}

    async def get_server(self, server_id):
        server = await ServerRepo(self.session).find_one(server_id)
        if not server:
            return None
        return self._to_dict(server)

    async def create_server(self, name, host, ssh_user, ssh_port=22, is_primary=False, is_active=True):
        repo = ServerRepo(self.session)
        if is_primary:
            await self._clear_primary(repo)
        server = await repo.create(name, host, ssh_user, ssh_port, is_primary, is_active)
        return self._to_dict(server)

    async def update_server(self, server_id, **payload):
        repo = ServerRepo(self.session)
        if payload.get("is_primary") is True:
            await self._clear_primary(repo)
        server = await repo.update(server_id, **payload)
        if not server:
            return None
        return self._to_dict(server)

    async def delete_server(self, server_id):
        return await ServerRepo(self.session).delete(server_id)

    async def check_server(self, server_id):
        repo = ServerRepo(self.session)
        server = await repo.find_one(server_id)
        if not server:
            return None
        status = await self._tcp_status(server.host, server.ssh_port)
        data = self._to_dict(server)
        data["status"] = status
        return data

    async def check_all(self):
        repo = ServerRepo(self.session)
        servers = await repo.find_all()
        checked = await asyncio.gather(*(self._with_status(server) for server in servers))
        return {"items": checked, "total": len(checked)}

    async def _with_status(self, server):
        data = self._to_dict(server)
        data["status"] = await self._tcp_status(server.host, server.ssh_port)
        return data

    async def _tcp_status(self, host, port):
        if not host or not port:
            return "unknown"
        try:
            reader, writer = await asyncio.wait_for(asyncio.open_connection(host, port), timeout=2)
            writer.close()
            await writer.wait_closed()
            return "online"
        except OSError:
            return "offline"
        except TimeoutError:
            return "timeout"

    async def _clear_primary(self, repo):
        servers = await repo.find_all()
        for server in servers:
            if server.is_primary:
                await repo.update(server.id, is_primary=False)

    @staticmethod
    def _to_dict(server):
        return {
            "id": server.id,
            "name": server.name,
            "host": server.host,
            "ssh_user": server.ssh_user,
            "ssh_port": server.ssh_port,
            "xray_container": server.xray_container,
            "awg_container": server.awg_container,
            "is_active": bool(server.is_active),
            "is_primary": bool(server.is_primary),
            "status": "unknown",
            "created_at": server.created_at.isoformat() if server.created_at else None,
        }
