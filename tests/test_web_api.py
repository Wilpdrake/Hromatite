async def auth_headers(client, username="alice"):
    await client.post("/api/admins", json={"username": username, "password": "password"})
    login = await client.post("/api/auth/login", json={"username": username, "password": "password"})
    return {"Authorization": f"Bearer {login.json()['token']}"}


async def test_admin_page_and_health(client):
    admin = await client.get("/admin")
    overview = await client.get("/admin/partials/overview")
    unknown_partial = await client.get("/admin/partials/unknown")
    health = await client.get("/api/health")

    assert admin.status_code == 200
    assert "Hromatite" in admin.text
    assert overview.status_code == 200
    assert "dashboard" in overview.text
    assert unknown_partial.status_code == 404
    assert health.status_code == 200
    assert health.json()["module"] == "web"


async def test_config_client_admin_and_log_api(client):
    headers = await auth_headers(client)
    config = await client.post("/api/configs", json={"name": "wg-main", "host": "vpn.example.com", "port": 51820}, headers=headers)
    configs = await client.get("/api/configs", headers=headers)
    toggled = await client.post(f"/api/configs/{config.json()['id']}/toggle", headers=headers)
    created_client = await client.post("/api/clients", json={"device_name": "phone", "allowed_ips": "10.0.0.2/32"}, headers=headers)
    clients = await client.get("/api/clients", headers=headers)
    admin = await client.post("/api/admins", json={"username": "bob", "password": "password"}, headers=headers)
    login = await client.post("/api/auth/login", json={"username": "alice", "password": "password"})
    logs = await client.get("/api/logs", headers=headers)

    assert config.status_code == 200
    assert configs.json()["total"] == 1
    assert toggled.json()["enabled"] is False
    assert created_client.status_code == 200
    assert clients.json()["total"] == 1
    assert admin.status_code == 200
    assert login.json()["username"] == "alice"
    assert login.json()["role"] == "admin"
    assert logs.status_code == 200


async def test_server_api_and_metrics(client, monkeypatch):
    from modules.web.services.server_service import ServerService

    async def fake_tcp_status(self, host, port):
        return "online"

    monkeypatch.setattr(ServerService, "_tcp_status", fake_tcp_status)

    headers = await auth_headers(client)
    created = await client.post("/api/servers", json={"name": "one", "host": "127.0.0.1", "ssh_user": "root", "ssh_port": 22}, headers=headers)
    server_id = created.json()["id"]
    servers = await client.get("/api/servers", headers=headers)
    checked = await client.get(f"/api/servers/{server_id}/check", headers=headers)
    primary = await client.post(f"/api/servers/{server_id}/primary", headers=headers)
    toggled = await client.post(f"/api/servers/{server_id}/toggle", headers=headers)
    metrics = await client.get("/metrics")
    deleted = await client.delete(f"/api/servers/{server_id}", headers=headers)

    assert created.status_code == 200
    assert servers.json()["total"] == 1
    assert checked.json()["status"] == "online"
    assert primary.json()["is_primary"] is True
    assert toggled.json()["is_active"] is False
    assert metrics.status_code == 200
    assert "hromatite_servers_total" in metrics.text
    assert deleted.json()["deleted"] is True


async def test_process_api(client):
    headers = await auth_headers(client)
    status = await client.get("/api/process", headers=headers)
    reload_wg = await client.post("/api/process/reload/wg", headers=headers)
    reload_xray = await client.post("/api/process/reload/xray", headers=headers)
    template = await client.get("/api/process/config-template", headers=headers)

    assert status.json()["wg_running"] is True
    assert reload_wg.json()["status"] == "ok"
    assert reload_xray.json()["status"] == "ok"
    assert "interface" in template.json()


async def test_api_requires_auth_and_bans_failed_logins(client):
    protected = await client.get("/api/stats")
    await client.post("/api/admins", json={"username": "alice", "password": "password"})

    failures = [
        await client.post("/api/auth/login", json={"username": "alice", "password": "wrong"})
        for _ in range(5)
    ]
    banned = await client.post("/api/auth/login", json={"username": "alice", "password": "password"})

    assert protected.status_code == 401
    assert failures[-1].status_code == 429
    assert banned.status_code == 429


async def test_register_creates_user_without_admin_access(client):
    registered = await client.post("/api/auth/register", json={"username": "guest", "password": "password"})
    login = await client.post("/api/auth/login", json={"username": "guest", "password": "password"})
    protected = await client.get("/api/stats", headers={"Authorization": f"Bearer {login.json()['token']}"})

    assert registered.status_code == 200
    assert registered.json()["role"] == "user"
    assert login.json()["role"] == "user"
    assert protected.status_code == 403
