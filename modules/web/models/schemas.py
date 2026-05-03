"""Pydantic схемы."""
from typing import Optional, List
from datetime import datetime
from pydantic import BaseModel


class LoginRequest(BaseModel):
    username: str
    password: str


class LoginResponse(BaseModel):
    token: str
    username: str
    role: str
    expires_in: int = 86400


class AdminInfo(BaseModel):
    id: int
    username: str
    role: str
    is_admin: bool
    is_active: bool
    created_at: Optional[str]


class AdminList(BaseModel):
    items: List[AdminInfo]
    total: int


class AdminCreate(BaseModel):
    username: str
    password: str
    role: str = "admin"


class RegisterRequest(BaseModel):
    username: str
    password: str


class ConfigItem(BaseModel):
    id: int
    name: str
    vpn_type: str
    host: str
    port: int
    enabled: bool
    created_at: Optional[str]
    updated_at: Optional[str]


class ConfigList(BaseModel):
    items: List[ConfigItem]
    total: int


class ConfigCreate(BaseModel):
    name: str
    vpn_type: str = "wireguard"
    host: str
    port: int


class ConfigUpdate(BaseModel):
    name: Optional[str] = None
    vpn_type: Optional[str] = None
    host: Optional[str] = None
    port: Optional[int] = None
    enabled: Optional[bool] = None


class ClientItem(BaseModel):
    id: int
    device_name: str
    public_key: Optional[str] = None
    allowed_ips: str
    enabled: bool
    last_handshake: Optional[str]
    bytes_in: int
    bytes_out: int


class ClientList(BaseModel):
    items: List[ClientItem]
    total: int


class ClientCreate(BaseModel):
    device_name: str
    allowed_ips: str = "0.0.0.0/0"


class ProcessStatus(BaseModel):
    wg_running: bool
    xray_running: bool
    wg_clients_online: int
    wg_clients_total: int
    wg_bytes_in: int
    wg_bytes_out: int


class ServerItem(BaseModel):
    id: int
    name: str
    host: str
    ssh_user: str
    ssh_port: int
    xray_container: Optional[str] = None
    awg_container: Optional[str] = None
    is_active: bool
    is_primary: bool
    status: str
    created_at: Optional[str] = None


class ServerCreate(BaseModel):
    name: str
    host: str
    ssh_user: str
    ssh_port: int = 22
    is_active: bool = True
    is_primary: bool = False


class ServerUpdate(BaseModel):
    name: Optional[str] = None
    host: Optional[str] = None
    ssh_user: Optional[str] = None
    ssh_port: Optional[int] = None
    is_active: Optional[bool] = None
    is_primary: Optional[bool] = None


class StatsOverview(BaseModel):
    total_configs: int
    active_configs: int
    total_clients: int
    active_clients: int
    total_servers: int
    wg_status: ProcessStatus


class LogItem(BaseModel):
    id: int
    username: Optional[str]
    level: str
    action: str
    target: Optional[str]
    details: Optional[str]
    created_at: Optional[str]


class LogList(BaseModel):
    items: List[LogItem]
    total: int


class WGConfig(BaseModel):
    private_key: str
    address: str
    dns: List[str]


class WGPeerConfig(BaseModel):
    private_key: str
    public_key: str
    preshared_key: str
    endpoint: str
    allowed_ips: str
    persistent_keepalive: int = 25


class WGServerConfig(BaseModel):
    interface: WGConfig
    server: dict
    peers: List[WGPeerConfig]
