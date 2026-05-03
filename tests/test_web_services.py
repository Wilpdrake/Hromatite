import pytest

from modules.web.services.admin_service import AdminService
from modules.web.services.client_service import ClientService
from modules.web.services.log_service import LogService
from modules.web.services.process_service import ProcessService
from modules.web.services.server_service import ServerService
from modules.web.services.stats_service import StatsService
from modules.web.services.vpn_service import VPNService


async def test_admin_service_creates_lists_and_logs_in_user(session):
    service = AdminService(session, "secret")

    created = await service.create_user("alice", "password")
    users = await service.list_users()
    login = await service.login("alice", "password")

    assert created["username"] == "alice"
    assert created["is_admin"] is True
    assert users["total"] == 1
    assert login["username"] == "alice"
    assert login["token"]


async def test_admin_service_rejects_duplicate_and_invalid_login(session):
    service = AdminService(session, "secret")
    await service.create_user("alice", "password")

    with pytest.raises(ValueError, match="существует"):
        await service.create_user("alice", "password")

    with pytest.raises(ValueError, match="Неверный"):
        await service.login("alice", "wrong")


async def test_vpn_service_crud_and_toggle(session):
    service = VPNService(session)

    config = await service.create_config("wg-main", "wireguard", "vpn.example.com", 51820)
    listed = await service.list_configs()
    toggled = await service.toggle_config(config.id)
    deleted = await service.delete_config(config.id)

    assert listed["total"] == 1
    assert listed["items"][0]["name"] == "wg-main"
    assert toggled.enabled is False
    assert deleted is True
    assert await service.get_config(config.id) is None


async def test_vpn_service_rejects_duplicate_name(session):
    service = VPNService(session)
    await service.create_config("wg-main", "wireguard", "vpn.example.com", 51820)

    with pytest.raises(ValueError, match="существует"):
        await service.create_config("wg-main", "wireguard", "vpn2.example.com", 51820)


async def test_client_service_crud(session):
    service = ClientService(session)

    client = await service.create_client("phone", telegram_id=123, allowed_ips="10.0.0.2/32")
    listed = await service.list_clients(telegram_id=123)
    deleted = await service.delete_client(client.id)

    assert listed["total"] == 1
    assert listed["items"][0]["device_name"] == "phone"
    assert listed["items"][0]["allowed_ips"] == "10.0.0.2/32"
    assert deleted is True


async def test_server_service_crud_primary_and_status(monkeypatch, session):
    async def fake_tcp_status(self, host, port):
        return "online" if host == "one.example.com" else "offline"

    monkeypatch.setattr(ServerService, "_tcp_status", fake_tcp_status)
    service = ServerService(session)

    first = await service.create_server("one", "one.example.com", "root", is_primary=True)
    second = await service.create_server("two", "two.example.com", "root", is_primary=True)
    listed = await service.list_servers()
    checked = await service.check_all()

    assert listed["total"] == 2
    assert (await service.get_server(first["id"]))["is_primary"] is False
    assert (await service.get_server(second["id"]))["is_primary"] is True
    assert [item["status"] for item in checked["items"]] == ["online", "offline"]
    assert await service.delete_server(first["id"]) is True


async def test_stats_process_and_logs_services(session):
    await VPNService(session).create_config("wg-main", "wireguard", "vpn.example.com", 51820)
    await ClientService(session).create_client("phone")
    await ServerService(session).create_server("one", "127.0.0.1", "root")
    await LogService(session).create("error", "create", "server", "failed", "admin")

    stats = await StatsService(session).get_overview()
    process = await ProcessService(session).get_status()
    logs = await LogService(session).get_counts_by_level()

    assert stats["total_configs"] == 1
    assert stats["active_configs"] == 1
    assert stats["total_clients"] == 1
    assert stats["total_servers"] == 1
    assert process["wg_running"] is True
    assert process["wg_clients_total"] == 1
    assert logs["error"] == 1
