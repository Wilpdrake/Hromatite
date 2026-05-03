"""Репозиторий логов."""
from sqlalchemy import select, func
from modules.web.models.base import AdminLog


class LogRepo:
    def __init__(self, session):
        self.session = session

    async def create_entry(self, level="info", action="", target=None, details=None, username=None):
        e = AdminLog(level=level, action=action, target=target, details=details, username=username)
        self.session.add(e)
        await self.session.commit()
        return e

    async def find_all(self, skip=0, limit=50, level=None, action=None):
        qc = select(func.count(AdminLog.id))
        if level:
            qc = qc.where(AdminLog.level == level)
        if action:
            qc = qc.where(AdminLog.action == action)
        total = (await self.session.execute(qc)).scalar() or 0

        q = select(AdminLog)
        if level:
            q = q.where(AdminLog.level == level)
        if action:
            q = q.where(AdminLog.action == action)
        q = q.order_by(AdminLog.created_at.desc()).offset(skip).limit(limit)
        r = await self.session.execute(q)
        return r.scalars().all(), total

    async def count_by_level(self):
        result = {}
        for lvl in ["info", "warning", "error"]:
            cnt = (await self.session.execute(
                select(func.count(AdminLog.id)).where(AdminLog.level == lvl)
            )).scalar() or 0
            result[lvl] = cnt
        return result
