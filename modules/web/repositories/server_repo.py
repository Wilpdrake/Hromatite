"""Репозиторий VPN серверов."""
import datetime

from sqlalchemy import select, func
from modules.web.models.base import VPNServer


class ServerRepo:
    def __init__(self, session):
        self.session = session

    async def find_all(self):
        r = await self.session.execute(select(VPNServer))
        return r.scalars().all()

    async def find_one(self, sid):
        r = await self.session.execute(
            select(VPNServer).where(VPNServer.id == sid)
        )
        return r.scalar_one_or_none()

    async def find_primary(self):
        r = await self.session.execute(
            select(VPNServer).where(VPNServer.is_primary == True)
        )
        return r.scalar_one_or_none()

    async def create(self, name, host, ssh_user, ssh_port=22, is_primary=False, is_active=True):
        s = VPNServer(name=name, host=host, ssh_user=ssh_user, ssh_port=ssh_port, is_primary=is_primary, is_active=is_active, created_at=datetime.datetime.now(datetime.UTC))
        self.session.add(s)
        await self.session.commit()
        await self.session.refresh(s)
        return s

    async def update(self, sid, **kw):
        r = await self.session.execute(
            select(VPNServer).where(VPNServer.id == sid)
        )
        s = r.scalar_one_or_none()
        if not s:
            return None
        for k, v in kw.items():
            if v is not None:
                setattr(s, k, v)
        await self.session.commit()
        await self.session.refresh(s)
        return s

    async def delete(self, sid):
        r = await self.session.execute(
            select(VPNServer).where(VPNServer.id == sid)
        )
        s = r.scalar_one_or_none()
        if not s:
            return False
        await self.session.delete(s)
        await self.session.commit()
        return True

    async def count(self):
        r = (await self.session.execute(
            select(func.count(VPNServer.id))
        )).scalar() or 0
        return r
