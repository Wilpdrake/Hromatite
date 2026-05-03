from jose import jwt

from modules.web.repositories.admin_repo import AdminUserRepo, hash_password


class AdminService:
    def __init__(self, session, secret_key):
        self.session = session
        self.secret_key = secret_key

    async def login(self, username, password):
        repo = AdminUserRepo(self.session)
        user = await repo.find_by_username(username)
        if not user or not repo.verify_password(password, user.password_hash):
            raise ValueError("Неверный логин или пароль")
        if not user.is_active:
            raise ValueError("Аккаунт заблокирован")
        role = user.role or ("admin" if user.is_admin else "user")
        token = jwt.encode(
            {"sub": str(user.id), "username": user.username, "role": role},
            self.secret_key,
            algorithm="HS256",
        )
        return {"token": token, "username": user.username, "role": role}

    async def create_user(self, username, password, role="admin"):
        repo = AdminUserRepo(self.session)
        if await repo.find_by_username(username):
            raise ValueError("Пользователь уже существует")
        role = role.lower()
        if role not in {"user", "admin", "superadmin", "owner"}:
            raise ValueError("Недопустимая роль")
        user = await repo.create(username, hash_password(password), is_admin=role in {"admin", "superadmin", "owner"}, role=role)
        return self._to_dict(user)

    async def register_user(self, username, password):
        return await self.create_user(username, password, role="user")

    async def list_users(self, skip=0, limit=100):
        repo = AdminUserRepo(self.session)
        users, total = await repo.find_all(skip=skip, limit=limit)
        return {"items": [self._to_dict(user) for user in users], "total": total}

    async def update_user(self, uid, **kwargs):
        repo = AdminUserRepo(self.session)
        user = await repo.update(uid, **kwargs)
        return self._to_dict(user) if user else None

    @staticmethod
    def _to_dict(user):
        return {
            "id": user.id,
            "username": user.username,
            "role": user.role or ("admin" if user.is_admin else "user"),
            "is_admin": user.is_admin,
            "is_active": user.is_active,
            "created_at": str(user.created_at) if user.created_at else None,
        }
