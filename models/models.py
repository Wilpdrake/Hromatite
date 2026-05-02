from datetime import datetime, timedelta
from sqlalchemy import Column, Integer, String, BigInteger, ForeignKey, Text, Float, DateTime
from sqlalchemy.orm import declarative_base, relationship

Base = declarative_base()

class User(Base):
    __tablename__ = 'users'

    id = Column(Integer, primary_key=True)
    telegram_id = Column(BigInteger, unique=True, nullable=False)
    username = Column(String, nullable=True)
    full_name = Column(String, nullable=True)
    balance = Column(Float, default=0.0)
    is_banned = Column(Integer, default=0)  # 0 - active, 1 - banned

    devices = relationship("Device", back_populates="user")
    payments = relationship("Payment", back_populates="user")

class Device(Base):
    __tablename__ = 'devices'

    id = Column(Integer, primary_key=True)
    user_id = Column(Integer, ForeignKey('users.id'))
    config_name = Column(String, nullable=False)   # имя устройства (Device-xxxx)
    config_data = Column(Text, nullable=False)      # полный текст .conf или vless:// ссылка
    vpn_type = Column(String, default="xray")    # xray, awg
    client_ip = Column(String, nullable=True)       # выданный IP
    client_public_key = Column(String, nullable=True)  # Public Key (Amnezia) или UUID (Xray)

    created_at = Column(DateTime, default=datetime.utcnow)
    expires_at = Column(DateTime, default=lambda: datetime.utcnow() + timedelta(days=30))

    user = relationship("User", back_populates="devices")

class Payment(Base):
    __tablename__ = 'payments'

    id = Column(Integer, primary_key=True)
    user_id = Column(Integer, ForeignKey('users.id'), nullable=False)
    provider = Column(String, nullable=False, default="manual")
    invoice_id = Column(String, nullable=True)
    amount = Column(Float, nullable=False)
    status = Column(String, nullable=False, default="pending")
    created_at = Column(DateTime, default=datetime.utcnow)
    paid_at = Column(DateTime, nullable=True)

    user = relationship("User", back_populates="payments")

class VpnServer(Base):
    __tablename__ = 'vpn_servers'

    id = Column(Integer, primary_key=True)
    name = Column(String, nullable=False, default="main")
    host = Column(String, nullable=False)
    ssh_user = Column(String, nullable=False)
    ssh_port = Column(Integer, nullable=False, default=22)
    xray_container = Column(String, nullable=False, default="hromatite-xray")
    awg_container = Column(String, nullable=False, default="hromatite-awg")
    is_active = Column(Integer, default=1)
    created_at = Column(DateTime, default=datetime.utcnow)