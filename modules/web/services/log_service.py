from modules.web.repositories.log_repo import LogRepo


class LogService:
    def __init__(self, session):
        self.session = session

    async def list_logs(self, skip=0, limit=50, level=None, action=None):
        repo = LogRepo(self.session)
        items, total = await repo.find_all(skip=skip, limit=limit, level=level, action=action)
        return {"items": [self._to_dict(item) for item in items], "total": total}

    async def create(self, level="info", action="", target=None, details=None, username=None):
        repo = LogRepo(self.session)
        return await repo.create_entry(level=level, action=action, target=target, details=details, username=username)

    async def get_counts_by_level(self):
        repo = LogRepo(self.session)
        return await repo.count_by_level()

    @staticmethod
    def _to_dict(item):
        return {
            "id": item.id,
            "username": item.username,
            "level": item.level,
            "action": item.action,
            "target": item.target,
            "details": item.details,
            "created_at": str(item.created_at) if item.created_at else None,
        }
