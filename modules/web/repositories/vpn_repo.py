"""Репозиторий VPN конфигов."""
from sqlalchemy import select, func
from modules.web.models.base import VPNConfig


class VPNRepo:
    def __init__(self, session):
        self.session = session

    async def find_all(self, skip=0, limit=100):
        cnt = (await self.session.execute(
            select(func.count(VPNConfig.id))
        )).scalar() or 0
        r = await self.session.execute(
            select(VPNConfig).offset(skip).limit(limit)
        )
        return r.scalars().all(), cnt

    async def find_one(self, cid):
        r = await self.session.execute(
            select(VPNConfig).where(VPNConfig.id == cid)
        )
        return r.scalar_one_or_none()

    async def find_by_name(self, name):
        r = await self.session.execute(
            select(VPNConfig).where(VPNConfig.name == name)
        )
        return r.scalar_one_or_none()

    async def create(self, name, vpn_type, host, port, config_data=None):
        cfg = VPNConfig(name=name, vpn_type=vpn_type, host=host, port=port, config_data=config_data)
        self.session.add(cfg)
        await self.session.commit()
        await self.session.refresh(cfg)
        return cfg

    async def update(self, cid, **kw):
        r = await self.session.execute(
            select(VPNConfig).where(VPNConfig.id == cid)
        )
        cfg = r.scalar_one_or_none()
        if not cfg:
            return None
        for k, v in kw.items():
            if v is not None:
                setattr(cfg, k, v)
        await self.session.commit()
        await self.session.refresh(cfg)
        return cfg

    async def delete(self, cid):
        r = await self.session.execute(
            select(VPNConfig).where(VPNConfig.id == cid)
        )
        cfg = r.scalar_one_or_none()
        if not cfg:
            return False
        await self.session.delete(cfg)
        await self.session.commit()
        return True

    async def get_enabled(self):
        r = await self.session.execute(
            select(VPNConfig).where(VPNConfig.enabled == True)
        )
        return r.scalars().all()

    async def count(self):
        r = (await self.session.execute(
            select(func.count(VPNConfig.id))
        )).scalar() or 0
        return r

    async def count_by_type(self, vpn_type):
        r = (await self.session.execute(
            select(func.count(VPNConfig.id)).where(VPNConfig.vpn_type == vpn_type)
        )).scalar() or 0
        return r
