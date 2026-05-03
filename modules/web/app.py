from pathlib import Path
from time import monotonic

from fastapi import Depends, FastAPI, HTTPException, Query, Request
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from fastapi.responses import HTMLResponse, PlainTextResponse
from fastapi.staticfiles import StaticFiles
from jose import JWTError, jwt
from sqlalchemy.ext.asyncio import AsyncSession

from core.config import Config
from core.database import create_session_maker
from modules.web.repositories.admin_repo import AdminUserRepo
from modules.web.models.schemas import AdminCreate, ClientCreate, ConfigCreate, ConfigUpdate, LoginRequest, RegisterRequest, ServerCreate, ServerUpdate
from modules.web.services.admin_service import AdminService
from modules.web.services.client_service import ClientService
from modules.web.services.log_service import LogService
from modules.web.services.process_service import ProcessService
from modules.web.services.server_service import ServerService
from modules.web.services.stats_service import StatsService
from modules.web.services.vpn_service import VPNService


def create_app(config: Config) -> FastAPI:
    app = FastAPI(title="Hromatite Admin", version="1.0.0")
    session_maker = create_session_maker(config.database_url)
    static_dir = Path(__file__).parent / "static"
    security = HTTPBearer(auto_error=False)
    auth_secret = config.web.admin_token or "change-me"
    request_log: dict[str, list[float]] = {}
    login_failures: dict[str, tuple[int, float]] = {}

    if static_dir.exists():
        app.mount("/static", StaticFiles(directory=static_dir), name="static")

    async def get_session():
        async with session_maker() as session:
            yield session

    def client_key(request: Request) -> str:
        forwarded = request.headers.get("x-forwarded-for")
        if forwarded:
            return forwarded.split(",")[0].strip()
        return request.client.host if request.client else "unknown"

    def enforce_rate_limit(request: Request):
        if config.web.rate_limit_requests <= 0:
            return
        now = monotonic()
        window = config.web.rate_limit_window_seconds
        key = client_key(request)
        timestamps = [item for item in request_log.get(key, []) if now - item < window]
        if len(timestamps) >= config.web.rate_limit_requests:
            raise HTTPException(status_code=429, detail="Слишком много запросов")
        timestamps.append(now)
        request_log[key] = timestamps

    async def authenticate_admin(request: Request, credentials: HTTPAuthorizationCredentials | None, session: AsyncSession):
        enforce_rate_limit(request)
        if not credentials:
            raise HTTPException(status_code=401, detail="Требуется авторизация")
        try:
            payload = jwt.decode(credentials.credentials, auth_secret, algorithms=["HS256"])
            user_id = int(payload.get("sub", "0"))
        except (JWTError, ValueError):
            raise HTTPException(status_code=401, detail="Недействительный токен")
        user = await AdminUserRepo(session).find_by_id(user_id)
        if not user or not user.is_active:
            raise HTTPException(status_code=401, detail="Аккаунт заблокирован")
        if (user.role or "user") not in {"admin", "superadmin", "owner"}:
            raise HTTPException(status_code=403, detail="Недостаточно прав")
        return user

    async def require_admin(
        request: Request,
        credentials: HTTPAuthorizationCredentials | None = Depends(security),
        session: AsyncSession = Depends(get_session),
    ):
        return await authenticate_admin(request, credentials, session)

    @app.get("/admin", response_class=HTMLResponse)
    async def admin_page():
        page = Path(__file__).parent / "template" / "admin.html"
        return page.read_text(encoding="utf-8")

    @app.get("/login", response_class=HTMLResponse)
    async def login_page():
        page = Path(__file__).parent / "template" / "auth.html"
        return page.read_text(encoding="utf-8")

    @app.get("/register", response_class=HTMLResponse)
    async def register_page():
        page = Path(__file__).parent / "template" / "auth.html"
        return page.read_text(encoding="utf-8")

    @app.get("/admin/partials/{partial_name}", response_class=HTMLResponse)
    async def admin_partial(partial_name: str):
        allowed_partials = {"overview", "vpn", "access", "system"}
        if partial_name not in allowed_partials:
            raise HTTPException(status_code=404, detail="Раздел админки не найден")
        page = Path(__file__).parent / "template" / "partials" / f"{partial_name}.html"
        return page.read_text(encoding="utf-8")

    @app.get("/api/health")
    async def health(session: AsyncSession = Depends(get_session)):
        process_status = await ProcessService(session).get_status()
        servers = await ServerService(session).check_all()
        unhealthy = [item for item in servers["items"] if item["is_active"] and item["status"] != "online"]
        return {
            "status": "degraded" if unhealthy else "ok",
            "module": "web",
            "process": process_status,
            "servers": servers,
        }

    @app.post("/api/auth/login")
    async def login(payload: LoginRequest, request: Request, session: AsyncSession = Depends(get_session)):
        enforce_rate_limit(request)
        key = client_key(request)
        attempts, banned_until = login_failures.get(key, (0, 0.0))
        now = monotonic()
        if banned_until > now:
            raise HTTPException(status_code=429, detail="Слишком много неудачных попыток входа")
        try:
            result = await AdminService(session, auth_secret).login(payload.username, payload.password)
            login_failures.pop(key, None)
            return result
        except ValueError as error:
            attempts += 1
            if attempts >= config.web.login_max_attempts:
                login_failures[key] = (attempts, now + config.web.login_ban_seconds)
                raise HTTPException(status_code=429, detail="Слишком много неудачных попыток входа") from error
            login_failures[key] = (attempts, 0.0)
            raise HTTPException(status_code=401, detail=str(error)) from error

    @app.post("/api/auth/register")
    async def register(payload: RegisterRequest, request: Request, session: AsyncSession = Depends(get_session)):
        enforce_rate_limit(request)
        try:
            created = await AdminService(session, auth_secret).register_user(payload.username, payload.password)
            return {"id": created["id"], "username": created["username"], "role": created["role"]}
        except ValueError as error:
            raise HTTPException(status_code=400, detail=str(error)) from error

    @app.get("/api/stats")
    async def stats(_admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        overview = await StatsService(session).get_overview()
        overview["process"] = await ProcessService(session).get_status()
        overview["logs"] = await LogService(session).get_counts_by_level()
        return overview

    @app.get("/api/admins")
    async def admins(skip: int = 0, limit: int = 100, _admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        return await AdminService(session, auth_secret).list_users(skip, limit)

    @app.post("/api/admins")
    async def create_admin(
        payload: AdminCreate,
        request: Request,
        credentials: HTTPAuthorizationCredentials | None = Depends(security),
        session: AsyncSession = Depends(get_session),
    ):
        users, total = await AdminUserRepo(session).find_all(skip=0, limit=1)
        if total:
            await authenticate_admin(request, credentials, session)
        else:
            enforce_rate_limit(request)
        try:
            return await AdminService(session, auth_secret).create_user(payload.username, payload.password, role=payload.role)
        except ValueError as error:
            raise HTTPException(status_code=400, detail=str(error)) from error

    @app.get("/api/configs")
    async def configs(skip: int = 0, limit: int = 100, _admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        return await VPNService(session).list_configs(skip, limit)

    @app.post("/api/configs")
    async def create_config(payload: ConfigCreate, _admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        try:
            config_item = await VPNService(session).create_config(payload.name, payload.vpn_type, payload.host, payload.port)
            return {"id": config_item.id, "name": config_item.name}
        except ValueError as error:
            raise HTTPException(status_code=400, detail=str(error)) from error

    @app.patch("/api/configs/{config_id}")
    async def update_config(config_id: int, payload: ConfigUpdate, _admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        config_item = await VPNService(session).update_config(config_id, **payload.model_dump(exclude_unset=True))
        if not config_item:
            raise HTTPException(status_code=404, detail="Конфиг не найден")
        return {"status": "ok"}

    @app.post("/api/configs/{config_id}/toggle")
    async def toggle_config(config_id: int, _admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        config_item = await VPNService(session).toggle_config(config_id)
        if not config_item:
            raise HTTPException(status_code=404, detail="Конфиг не найден")
        return {"status": "ok", "enabled": config_item.enabled}

    @app.delete("/api/configs/{config_id}")
    async def delete_config(config_id: int, _admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        return {"deleted": await VPNService(session).delete_config(config_id)}

    @app.get("/api/clients")
    async def clients(
        telegram_id: int | None = Query(default=None),
        skip: int = 0,
        limit: int = 100,
        _admin=Depends(require_admin),
        session: AsyncSession = Depends(get_session),
    ):
        return await ClientService(session).list_clients(telegram_id, skip, limit)

    @app.post("/api/clients")
    async def create_client(payload: ClientCreate, _admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        client = await ClientService(session).create_client(payload.device_name, allowed_ips=payload.allowed_ips)
        return {"id": client.id, "device_name": client.device_name}

    @app.delete("/api/clients/{client_id}")
    async def delete_client(client_id: int, _admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        return {"deleted": await ClientService(session).delete_client(client_id)}

    @app.get("/api/servers")
    async def servers(_admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        return await ServerService(session).list_servers()

    @app.post("/api/servers")
    async def create_server(payload: ServerCreate, _admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        return await ServerService(session).create_server(
            payload.name,
            payload.host,
            payload.ssh_user,
            payload.ssh_port,
            payload.is_primary,
            payload.is_active,
        )

    @app.patch("/api/servers/{server_id}")
    async def update_server(server_id: int, payload: ServerUpdate, _admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        server = await ServerService(session).update_server(server_id, **payload.model_dump(exclude_unset=True))
        if not server:
            raise HTTPException(status_code=404, detail="Сервер не найден")
        return server

    @app.post("/api/servers/{server_id}/toggle")
    async def toggle_server(server_id: int, _admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        service = ServerService(session)
        server = await service.get_server(server_id)
        if not server:
            raise HTTPException(status_code=404, detail="Сервер не найден")
        updated = await service.update_server(server_id, is_active=not server["is_active"])
        return {"status": "ok", "is_active": updated["is_active"]}

    @app.post("/api/servers/{server_id}/primary")
    async def make_primary_server(server_id: int, _admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        server = await ServerService(session).update_server(server_id, is_primary=True)
        if not server:
            raise HTTPException(status_code=404, detail="Сервер не найден")
        return {"status": "ok", "is_primary": server["is_primary"]}

    @app.get("/api/servers/{server_id}/check")
    async def check_server(server_id: int, _admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        server = await ServerService(session).check_server(server_id)
        if not server:
            raise HTTPException(status_code=404, detail="Сервер не найден")
        return server

    @app.delete("/api/servers/{server_id}")
    async def delete_server(server_id: int, _admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        return {"deleted": await ServerService(session).delete_server(server_id)}

    @app.get("/api/process")
    async def process(_admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        return await ProcessService(session).get_status()

    @app.post("/api/process/reload/wg")
    async def reload_wg(_admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        return await ProcessService(session).reload_wg()

    @app.post("/api/process/reload/xray")
    async def reload_xray(_admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        return await ProcessService(session).reload_xray()

    @app.get("/api/process/config-template")
    async def config_template(_admin=Depends(require_admin), session: AsyncSession = Depends(get_session)):
        return await ProcessService(session).get_vpn_config_gen()

    @app.get("/api/logs")
    async def logs(
        skip: int = 0,
        limit: int = 50,
        level: str | None = None,
        action: str | None = None,
        _admin=Depends(require_admin),
        session: AsyncSession = Depends(get_session),
    ):
        return await LogService(session).list_logs(skip, limit, level, action)

    @app.get("/metrics", response_class=PlainTextResponse)
    async def metrics(session: AsyncSession = Depends(get_session)):
        overview = await StatsService(session).get_overview()
        process_status = await ProcessService(session).get_status()
        servers_status = await ServerService(session).check_all()
        online_servers = len([item for item in servers_status["items"] if item["status"] == "online"])
        lines = [
            "# HELP hromatite_configs_total Total VPN configs",
            "# TYPE hromatite_configs_total gauge",
            f"hromatite_configs_total {overview['total_configs']}",
            "# HELP hromatite_configs_active Active VPN configs",
            "# TYPE hromatite_configs_active gauge",
            f"hromatite_configs_active {overview['active_configs']}",
            "# HELP hromatite_clients_total Total clients",
            "# TYPE hromatite_clients_total gauge",
            f"hromatite_clients_total {overview['total_clients']}",
            "# HELP hromatite_servers_total Total servers",
            "# TYPE hromatite_servers_total gauge",
            f"hromatite_servers_total {overview['total_servers']}",
            "# HELP hromatite_servers_online Online servers by SSH TCP check",
            "# TYPE hromatite_servers_online gauge",
            f"hromatite_servers_online {online_servers}",
            "# HELP hromatite_wg_running WireGuard process status",
            "# TYPE hromatite_wg_running gauge",
            f"hromatite_wg_running {int(process_status['wg_running'])}",
            "# HELP hromatite_xray_running Xray process status",
            "# TYPE hromatite_xray_running gauge",
            f"hromatite_xray_running {int(process_status['xray_running'])}",
        ]
        return "\n".join(lines) + "\n"

    return app
