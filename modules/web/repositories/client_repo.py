"""Репозиторий WG клиентов."""
import base64
import os
from sqlalchemy import select, func
from modules.web.models.base import ClientDevice


class ClientRepo:
    def __init__(self, session):
        self.session = session

    async def find_all(self, telegram_id=None, skip=0, limit=100):
        q = select(ClientDevice)
        if telegram_id is not None:
            q = q.where(ClientDevice.telegram_id == telegram_id)
        r = await self.session.execute(q.offset(skip).limit(limit))
        items = r.scalars().all()

        qc = select(func.count(ClientDevice.id))
        if telegram_id is not None:
            qc = qc.where(ClientDevice.telegram_id == telegram_id)
        total = (await self.session.execute(qc)).scalar() or 0
        return items, total

    async def find_one(self, cid):
        r = await self.session.execute(
            select(ClientDevice).where(ClientDevice.id == cid)
        )
        return r.scalar_one_or_none()

    async def create(self, device_name, telegram_id=None, allowed_ips="0.0.0.0/0", enabled=True):
        pk, sk = self._generate_keys()
        c = ClientDevice(
            telegram_id=telegram_id,
            device_name=device_name,
            public_key=pk,
            preshared_key=sk,
            allowed_ips=allowed_ips,
            enabled=enabled,
        )
        self.session.add(c)
        await self.session.commit()
        await self.session.refresh(c)
        return c

    async def update(self, cid, **kw):
        r = await self.session.execute(
            select(ClientDevice).where(ClientDevice.id == cid)
        )
        c = r.scalar_one_or_none()
        if not c:
            return None
        for k, v in kw.items():
            if v is not None:
                setattr(c, k, v)
        await self.session.commit()
        await self.session.refresh(c)
        return c

    async def delete(self, cid):
        r = await self.session.execute(
            select(ClientDevice).where(ClientDevice.id == cid)
        )
        c = r.scalar_one_or_none()
        if not c:
            return False
        await self.session.delete(c)
        await self.session.commit()
        return True

    def _generate_keys(self):
        sk = base64.b64encode(os.urandom(32)).decode()
        pk = base64.b64encode(os.urandom(32)).decode()
        return pk, sk
