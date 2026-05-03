"""SQLAlchemy модели админки."""
from sqlalchemy import Column, Integer, String, Boolean, DateTime, Text, BigInteger
from sqlalchemy.orm import declarative_base


Base = declarative_base()


class AdminUser(Base):
    __tablename__ = "admin_users"

    id = Column(Integer, primary_key=True)
    username = Column(String(50), unique=True, nullable=False)
    password_hash = Column(String(255), nullable=False)
    role = Column(String(20), default="user", nullable=False)
    is_admin = Column(Boolean, default=False)
    is_active = Column(Boolean, default=True)
    created_at = Column(DateTime)


class VPNConfig(Base):
    """Конфигурация VPN."""

    __tablename__ = "vpn_configs"

    id = Column(Integer, primary_key=True)
    name = Column(String(100), nullable=False)
    vpn_type = Column(String(20), default="wireguard")
    host = Column(String(255), nullable=False)
    port = Column(Integer, nullable=False)
    enabled = Column(Boolean, default=True)
    config_data = Column(Text)
    created_at = Column(DateTime)
    updated_at = Column(DateTime)


class ClientDevice(Base):
    """WG клиент."""

    __tablename__ = "client_devices"

    id = Column(Integer, primary_key=True)
    telegram_id = Column(BigInteger)
    device_name = Column(String(100), nullable=False)
    public_key = Column(String(255))
    preshared_key = Column(String(255))
    allowed_ips = Column(String(255), default="0.0.0.0/0")
    enabled = Column(Boolean, default=True)
    last_handshake = Column(DateTime)
    bytes_in = Column(BigInteger, default=0)
    bytes_out = Column(BigInteger, default=0)
    config_raw = Column(Text)
    created_at = Column(DateTime)


class AdminLog(Base):
    """Лог действий."""

    __tablename__ = "admin_logs"

    id = Column(Integer, primary_key=True)
    user_id = Column(Integer)
    username = Column(String(50))
    level = Column(String(10), default="info")
    action = Column(String(50), default="")
    target = Column(String(100))
    details = Column(Text)
    created_at = Column(DateTime)


class VPNServer(Base):
    """Сервер VPN."""

    __tablename__ = "vpn_servers"

    id = Column(Integer, primary_key=True)
    name = Column(String(100), nullable=False)
    host = Column(String(255), nullable=False)
    ssh_user = Column(String(50), nullable=False)
    ssh_port = Column(Integer, default=22)
    xray_container = Column(String(100), default="hromatite-xray")
    awg_container = Column(String(100), default="hromatite-awg")
    is_active = Column(Boolean, default=True)
    is_primary = Column(Boolean, default=False)
    created_at = Column(DateTime)
