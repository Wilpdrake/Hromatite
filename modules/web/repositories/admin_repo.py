"""Репозиторий пользователей."""
from sqlalchemy import func, select
from passlib.hash import bcrypt

from modules.web.models.base import AdminUser


class AdminUserRepo:
    def __init__(self, session):
        self.session = session

    async def find_by_username(self, username):
        r = await self.session.execute(
            select(AdminUser).where(AdminUser.username == username)
        )
        return r.scalar_one_or_none()

    async def find_by_id(self, uid):
        r = await self.session.execute(
            select(AdminUser).where(AdminUser.id == uid)
        )
        return r.scalar_one_or_none()

    async def find_all(self, skip=0, limit=100):
        cnt = (await self.session.execute(select(func.count(AdminUser.id)))).scalar() or 0
        r = await self.session.execute(
            select(AdminUser).offset(skip).limit(limit)
        )
        return r.scalars().all(), cnt

    async def create(self, username, password_hash, is_admin=False, role="user"):
        u = AdminUser(username=username, password_hash=password_hash, is_admin=is_admin, role=role)
        self.session.add(u)
        await self.session.commit()
        await self.session.refresh(u)
        return u

    async def update(self, uid, **kw):
        r = await self.session.execute(
            select(AdminUser).where(AdminUser.id == uid)
        )
        u = r.scalar_one_or_none()
        if not u:
            return None
        for k, v in kw.items():
            if v is not None:
                setattr(u, k, v)
        await self.session.commit()
        await self.session.refresh(u)
        return u

    async def delete(self, uid):
        r = await self.session.execute(
            select(AdminUser).where(AdminUser.id == uid)
        )
        u = r.scalar_one_or_none()
        if not u or u.username == "admin":
            return False
        await self.session.delete(u)
        await self.session.commit()
        return True

    @staticmethod
    def verify_password(password, hash_val):
        return bcrypt.verify(password, hash_val)


def hash_password(password):
    return bcrypt.hash(password)
