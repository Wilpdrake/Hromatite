# Памятка SSL для nginx + Cloudflare

Эта памятка описывает, как позже включить HTTPS для схемы `Cloudflare -> nginx -> WireGuard -> FastAPI`.

## Рекомендуемый вариант

Для production используй Cloudflare SSL/TLS mode `Full (strict)` и origin-сертификат Cloudflare на nginx.

## Шаги в Cloudflare

1. Открой домен в Cloudflare.
2. Перейди в `SSL/TLS -> Overview`.
3. Для тестов можно временно использовать `Flexible` или `Full`, но для production выбери `Full (strict)`.
4. Перейди в `SSL/TLS -> Origin Server`.
5. Создай Origin Certificate для домена и поддоменов.
6. Сохрани certificate и private key.

## Установка сертификата на сервер

На удалённом сервере:

```bash
sudo mkdir -p /etc/nginx/ssl/hromatite
sudo nano /etc/nginx/ssl/hromatite/origin.crt
sudo nano /etc/nginx/ssl/hromatite/origin.key
sudo chmod 600 /etc/nginx/ssl/hromatite/origin.key
sudo chmod 644 /etc/nginx/ssl/hromatite/origin.crt
```

## Пример HTTPS server block

Замени временный `listen 443` блок в `docs/deploy/nginx-hromatite.conf` на:

```nginx
server {
    listen 443 ssl http2;
    server_name example.com;

    ssl_certificate /etc/nginx/ssl/hromatite/origin.crt;
    ssl_certificate_key /etc/nginx/ssl/hromatite/origin.key;

    client_max_body_size 20m;

    location / {
        proxy_pass http://10.66.66.2:8000;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto https;
        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection "upgrade";
        proxy_read_timeout 300s;
        proxy_connect_timeout 30s;
        proxy_send_timeout 300s;
    }
}
```

HTTP можно перенаправить на HTTPS:

```nginx
server {
    listen 80;
    server_name example.com;
    return 301 https://$host$request_uri;
}
```

## Проверка

```bash
sudo nginx -t
sudo systemctl reload nginx
curl -I https://example.com/admin
```

## Важные замечания

- Не коммить реальные private keys и сертификаты в репозиторий.
- Cloudflare Origin Certificate доверяется Cloudflare, но не обычным браузером напрямую.
- Если Cloudflare proxy выключен, используй публичный сертификат Let's Encrypt вместо Origin Certificate.
