from modules.web.models.base import AdminLog, AdminUser, Base, ClientDevice, VPNConfig, VPNServer

BaseModel = Base
VPNConfigModel = VPNConfig


__all__ = [
    "BaseModel",
    "AdminUser",
    "VPNConfigModel",
    "ClientDevice",
    "AdminLog",
    "Base",
    "VPNConfig",
    "VPNServer",
]
