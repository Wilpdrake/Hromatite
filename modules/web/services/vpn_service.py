from modules.web.repositories.vpn_repo import VPNRepo


class VPNService:
    def __init__(self, session):
        self.session = session

    async def list_configs(self, skip=0, limit=100):
        repo = VPNRepo(self.session)
        configs, total = await repo.find_all(skip=skip, limit=limit)
        return {"items": [self._to_dict(config) for config in configs], "total": total}

    async def get_config(self, cid):
        repo = VPNRepo(self.session)
        config = await repo.find_one(cid)
        return self._to_dict(config) if config else None

    async def create_config(self, name, vpn_type, host, port):
        repo = VPNRepo(self.session)
        if await repo.find_by_name(name):
            raise ValueError("Конфиг с таким именем уже существует")
        return await repo.create(name, vpn_type, host, port)

    async def update_config(self, cid, **kwargs):
        repo = VPNRepo(self.session)
        return await repo.update(cid, **kwargs)

    async def delete_config(self, cid):
        repo = VPNRepo(self.session)
        return await repo.delete(cid)

    async def toggle_config(self, cid):
        repo = VPNRepo(self.session)
        config = await repo.find_one(cid)
        if not config:
            return None
        return await self.update_config(cid, enabled=not config.enabled)

    @staticmethod
    def _to_dict(config):
        return {
            "id": config.id,
            "name": config.name,
            "vpn_type": config.vpn_type,
            "host": config.host,
            "port": config.port,
            "enabled": config.enabled,
            "created_at": str(config.created_at) if config.created_at else None,
            "updated_at": str(config.updated_at) if config.updated_at else None,
        }
