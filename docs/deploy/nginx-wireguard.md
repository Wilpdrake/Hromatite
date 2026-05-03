# Деплой nginx + WireGuard для Hromatite

Цель схемы: удалённый сервер принимает внешний HTTP/HTTPS-трафик от Cloudflare на `80/443`, а затем проксирует его через приватный WireGuard-туннель на локальный ПК, где FastAPI слушает порт `8000`.

## Схема

```text
Cloudflare
  -> remote server public IP:80/443
  -> nginx on remote server
  -> WireGuard tunnel 10.66.66.1/24 -> 10.66.66.2:8000
  -> local PC FastAPI
```

## Порты

- `80/tcp` — HTTP от Cloudflare до nginx.
- `443/tcp` — HTTPS от Cloudflare до nginx, пока без сертификатов в шаблоне.
- `51820/udp` — WireGuard на удалённом сервере.
- `8000/tcp` — FastAPI на локальном ПК, доступен серверу через WireGuard IP `10.66.66.2`.

## Файлы

- `docs/deploy/nginx-hromatite.conf` — шаблон nginx reverse proxy.
- `docs/deploy/wireguard-server.conf.example` — пример `/etc/wireguard/wg0.conf` на сервере.
- `docs/deploy/wireguard-client-windows.conf.example` — пример конфига WireGuard на локальном ПК.
- `docs/deploy/ssl-cloudflare.md` — памятка по включению SSL позже.

## Подготовка ключей WireGuard

На Linux-сервере:

```bash
umask 077
wg genkey | tee server_private.key | wg pubkey > server_public.key
wg genkey | tee client_private.key | wg pubkey > client_public.key
cat server_private.key server_public.key client_private.key client_public.key
```

Подставь ключи в server/client конфиги:

- `SERVER_PRIVATE_KEY` — private key сервера.
- `SERVER_PUBLIC_KEY` — public key сервера.
- `CLIENT_PRIVATE_KEY` — private key локального ПК.
- `CLIENT_PUBLIC_KEY` — public key локального ПК.

## Деплой WireGuard на удалённом сервере

1. Установи WireGuard:

```bash
sudo apt update
sudo apt install -y wireguard
```

2. Скопируй `docs/deploy/wireguard-server.conf.example` в `/etc/wireguard/wg0.conf`.
3. Замени placeholders на реальные ключи.
4. Включи IP forwarding:

```bash
echo 'net.ipv4.ip_forward=1' | sudo tee /etc/sysctl.d/99-hromatite-wireguard.conf
sudo sysctl --system
```

5. Открой UDP-порт WireGuard:

```bash
sudo ufw allow 51820/udp
```

6. Запусти туннель:

```bash
sudo systemctl enable --now wg-quick@wg0
sudo wg show
```

## Деплой nginx на удалённом сервере

1. Установи nginx:

```bash
sudo apt update
sudo apt install -y nginx
```

2. Скопируй шаблон:

```bash
sudo cp docs/deploy/nginx-hromatite.conf /etc/nginx/sites-available/hromatite.conf
sudo ln -s /etc/nginx/sites-available/hromatite.conf /etc/nginx/sites-enabled/hromatite.conf
```

3. Замени `example.com` на домен проекта.
4. Проверь и перезагрузи nginx:

```bash
sudo nginx -t
sudo systemctl reload nginx
```

## Запуск FastAPI на локальном ПК

На локальном ПК приложение должно слушать порт `8000` на интерфейсе, доступном из WireGuard. Для Hromatite это обычно:

```powershell
$env:WEB_HOST = "0.0.0.0"
$env:WEB_PORT = "8000"
python run_web.py
```

Проверка с удалённого сервера:

```bash
curl http://10.66.66.2:8000/admin
```

Если ответа нет, проверь:

- WireGuard подключён на локальном ПК.
- Windows Firewall разрешает входящие подключения на `8000/tcp` для WireGuard-интерфейса.
- FastAPI слушает `0.0.0.0:8000`, а не только `127.0.0.1:8000`.

## Проверка снаружи

После настройки DNS в Cloudflare на публичный IP сервера:

```bash
curl -I http://example.com/admin
```

До настройки SSL используй Cloudflare SSL/TLS mode `Off` или временно проксируй только HTTP. Для production смотри `docs/deploy/ssl-cloudflare.md`.
