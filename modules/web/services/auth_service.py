from time import monotonic
from urllib.parse import quote

from fastapi import HTTPException, Request
from fastapi.responses import RedirectResponse
from jose import JWTError, jwt

from modules.web.repositories.admin_repo import AdminUserRepo


ADMIN_ROLES = {"admin", "superadmin", "owner"}


class AuthManager:
    def __init__(self, config, secret_key):
        self.config = config
        self.secret_key = secret_key
        self.request_log: dict[str, list[float]] = {}
        self.login_failures: dict[str, tuple[int, float]] = {}

    def client_key(self, request: Request) -> str:
        forwarded = request.headers.get("x-forwarded-for")
        if forwarded:
            return forwarded.split(",")[0].strip()
        return request.client.host if request.client else "unknown"

    def enforce_rate_limit(self, request: Request):
        if self.config.web.rate_limit_requests <= 0:
            return
        now = monotonic()
        window = self.config.web.rate_limit_window_seconds
        key = self.client_key(request)
        timestamps = [item for item in self.request_log.get(key, []) if now - item < window]
        if len(timestamps) >= self.config.web.rate_limit_requests:
            raise HTTPException(status_code=429, detail="Слишком много запросов")
        timestamps.append(now)
        self.request_log[key] = timestamps

    def read_token(self, request: Request, credentials=None) -> str | None:
        if credentials:
            return credentials.credentials
        return request.cookies.get("adminToken")

    async def authenticate_user(self, request: Request, session, credentials=None):
        self.enforce_rate_limit(request)
        token = self.read_token(request, credentials)
        if not token:
            raise HTTPException(status_code=401, detail="Требуется авторизация")
        try:
            payload = jwt.decode(token, self.secret_key, algorithms=["HS256"])
            user_id = int(payload.get("sub", "0"))
        except (JWTError, ValueError):
            raise HTTPException(status_code=401, detail="Недействительный токен")
        user = await AdminUserRepo(session).find_by_id(user_id)
        if not user or not user.is_active:
            raise HTTPException(status_code=401, detail="Аккаунт заблокирован")
        return user

    async def authenticate_roles(self, request: Request, session, credentials=None, roles=ADMIN_ROLES):
        user = await self.authenticate_user(request, session, credentials)
        if (user.role or "user") not in roles:
            raise HTTPException(status_code=403, detail="Недостаточно прав")
        return user

    def register_login_failure(self, request: Request):
        key = self.client_key(request)
        attempts, _banned_until = self.login_failures.get(key, (0, 0.0))
        attempts += 1
        now = monotonic()
        if attempts >= self.config.web.login_max_attempts:
            self.login_failures[key] = (attempts, now + self.config.web.login_ban_seconds)
            raise HTTPException(status_code=429, detail="Слишком много неудачных попыток входа")
        self.login_failures[key] = (attempts, 0.0)

    def ensure_login_allowed(self, request: Request):
        key = self.client_key(request)
        _attempts, banned_until = self.login_failures.get(key, (0, 0.0))
        if banned_until > monotonic():
            raise HTTPException(status_code=429, detail="Слишком много неудачных попыток входа")

    def clear_login_failures(self, request: Request):
        self.login_failures.pop(self.client_key(request), None)

    async def require_html_roles(self, request: Request, session, roles=ADMIN_ROLES):
        try:
            return await self.authenticate_roles(request, session, roles=roles)
        except HTTPException as error:
            if error.status_code in {401, 403}:
                return RedirectResponse(f"/login?next={quote(str(request.url.path))}", status_code=303)
            raise
