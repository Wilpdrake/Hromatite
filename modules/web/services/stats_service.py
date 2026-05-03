from modules.web.repositories.client_repo import ClientRepo
from modules.web.repositories.server_repo import ServerRepo
from modules.web.repositories.vpn_repo import VPNRepo


class StatsService:
    def __init__(self, session):
        self.session = session

    async def get_overview(self):
        vpn_repo = VPNRepo(self.session)
        client_repo = ClientRepo(self.session)
        server_repo = ServerRepo(self.session)
        total_clients = (await client_repo.find_all(limit=1))[1]
        servers = await server_repo.find_all()
        return {
            "total_configs": await vpn_repo.count(),
            "active_configs": len(await vpn_repo.get_enabled()),
            "total_clients": total_clients,
            "active_clients": 0,
            "total_servers": len(servers),
        }
