const api = async (url, options = {}) => {
  const token = localStorage.getItem('adminToken');
  const headers = { 'Content-Type': 'application/json', ...(options.headers || {}) };
  if (token) headers.Authorization = `Bearer ${token}`;
  const response = await fetch(url, { ...options, headers });
  if (response.status === 401) {
    localStorage.removeItem('adminToken');
    document.cookie = 'adminToken=; Path=/; Max-Age=0; SameSite=Lax';
    window.location.href = `/login?next=${encodeURIComponent(window.location.pathname)}`;
  }
  if (response.status === 403) window.location.href = `/login?next=${encodeURIComponent(window.location.pathname)}`;
  if (!response.ok) throw new Error(await response.text());
  return response.json();
};

const cell = value => value ?? '—';
const status = value => `<span class="pill ${value ? 'ok' : 'bad'}">${value ? 'active' : 'off'}</span>`;
let activeTab = 'overview';
let activeUrl = '/admin/partials/overview';

function renderStats(data) {
  const items = [
    ['Конфиги', data.total_configs],
    ['Активные', data.active_configs],
    ['Клиенты', data.total_clients],
    ['Серверы', data.total_servers],
    ['Ошибки', data.logs?.error ?? 0],
  ];
  document.querySelector('#dashboard').innerHTML = items.map(([label, value]) => `<article class="card"><span>${label}</span><strong>${value}</strong></article>`).join('');
}

function renderTable(target, headers, rows) {
  const columns = `repeat(${headers.length}, minmax(130px, 1fr))`;
  document.querySelector(target).innerHTML = [
    `<div class="thead" style="grid-template-columns:${columns}">${headers.map(item => `<span>${item}</span>`).join('')}</div>`,
    ...rows,
  ].join('').replaceAll('<div class="row">', `<div class="row" style="grid-template-columns:${columns}">`);
}

async function loadConfigs() {
  const data = await api('/api/configs');
  renderTable('#configsTable', ['Название', 'Тип', 'Хост', 'Статус', 'Действия'], data.items.map(item => `
    <div class="row">
      <span>${cell(item.name)}</span><span>${cell(item.vpn_type)}</span><span>${cell(item.host)}:${cell(item.port)}</span><span>${status(item.enabled)}</span>
      <span><button onclick="toggleConfig(${item.id})">Toggle</button> <button onclick="deleteConfig(${item.id})">Delete</button></span>
    </div>`));
}

async function loadClients() {
  const data = await api('/api/clients');
  renderTable('#clientsTable', ['Устройство', 'Public key', 'Allowed IPs', 'Статус', 'Трафик'], data.items.map(item => `
    <div class="row">
      <span>${cell(item.device_name)}</span><span>${cell(item.public_key)}</span><span>${cell(item.allowed_ips)}</span><span>${status(item.enabled)}</span>
      <span>${cell(item.bytes_in)} / ${cell(item.bytes_out)}</span>
    </div>`));
}

async function loadServers() {
  const data = await api('/api/servers');
  renderTable('#serversTable', ['Название', 'Host', 'SSH', 'Primary', 'Статус', 'Действия'], data.items.map(item => `
    <div class="row">
      <span>${cell(item.name)}</span><span>${cell(item.host)}</span><span>${cell(item.ssh_user)}:${cell(item.ssh_port)}</span><span>${status(item.is_primary)}</span><span>${status(item.is_active)} / ${cell(item.status)}</span>
      <span><button onclick="checkServer(${item.id})">Check</button> <button onclick="makePrimaryServer(${item.id})">Primary</button> <button onclick="toggleServer(${item.id})">Toggle</button> <button onclick="deleteServer(${item.id})">Delete</button></span>
    </div>`));
}

async function loadAdmins() {
  const data = await api('/api/admins');
  renderTable('#adminsTable', ['Логин', 'Admin', 'Активен', 'Создан', 'ID'], data.items.map(item => `
    <div class="row"><span>${item.username}</span><span>${status(item.is_admin)}</span><span>${status(item.is_active)}</span><span>${cell(item.created_at)}</span><span>${item.id}</span></div>`));
}

async function loadLogs() {
  const data = await api('/api/logs');
  renderTable('#logsTable', ['Уровень', 'Действие', 'Цель', 'Дата', 'Детали'], data.items.map(item => `
    <div class="row"><span>${item.level}</span><span>${cell(item.action)}</span><span>${cell(item.target)}</span><span>${cell(item.created_at)}</span><span>${cell(item.details)}</span></div>`));
}

async function loadProcess() {
  document.querySelector('#processBox').textContent = JSON.stringify(await api('/api/process'), null, 2);
}

async function loadHealth() {
  document.querySelector('#healthBox').textContent = JSON.stringify(await api('/api/health'), null, 2);
}

async function loadOverview() {
  const stats = await api('/api/stats');
  renderStats(stats);
  await loadHealth();
}

async function loadVpn() {
  await Promise.all([loadConfigs(), loadServers(), loadClients()]);
}

async function loadAccess() {
  await loadAdmins();
}

async function loadSystem() {
  await Promise.all([loadLogs(), loadProcess()]);
}

const tabLoaders = {
  overview: loadOverview,
  vpn: loadVpn,
  access: loadAccess,
  system: loadSystem,
};

const forms = {
  configForm: '/api/configs',
  serverForm: '/api/servers',
  clientForm: '/api/clients',
  adminForm: '/api/admins',
};

async function loadPartial(url) {
  const response = await fetch(url);
  if (!response.ok) throw new Error(await response.text());
  document.querySelector('#partialRoot').innerHTML = await response.text();
  document.querySelectorAll('.tab-panel').forEach(panel => panel.classList.add('active'));
  bindActivePanel();
}

async function refresh() {
  await tabLoaders[activeTab]();
}

async function activateTab(tab, url) {
  activeTab = tab;
  activeUrl = url;
  document.querySelectorAll('.tab-link').forEach(button => button.classList.toggle('active', button.dataset.tab === tab));
  await loadPartial(url);
  await refresh();
}

async function toggleConfig(id) {
  await api(`/api/configs/${id}/toggle`, { method: 'POST' });
  await refresh();
}

async function deleteConfig(id) {
  await api(`/api/configs/${id}`, { method: 'DELETE' });
  await refresh();
}

async function toggleServer(id) {
  await api(`/api/servers/${id}/toggle`, { method: 'POST' });
  await refresh();
}

async function makePrimaryServer(id) {
  await api(`/api/servers/${id}/primary`, { method: 'POST' });
  await refresh();
}

async function checkServer(id) {
  alert(JSON.stringify(await api(`/api/servers/${id}/check`), null, 2));
  await refresh();
}

async function deleteServer(id) {
  await api(`/api/servers/${id}`, { method: 'DELETE' });
  await refresh();
}

function bindForm(form, url) {
  form.addEventListener('submit', async event => {
    event.preventDefault();
    const data = Object.fromEntries(new FormData(event.target));
    if (data.port) data.port = Number(data.port);
    if (data.ssh_port) data.ssh_port = Number(data.ssh_port);
    await api(url, { method: 'POST', body: JSON.stringify(data) });
    event.target.reset();
    await refresh();
  });
}

function bindActivePanel() {
  Object.entries(forms).forEach(([id, url]) => {
    const form = document.querySelector(`#${id}`);
    if (form) bindForm(form, url);
  });
  document.querySelectorAll('[data-action]').forEach(button => button.addEventListener('click', async () => {
    await api(button.dataset.action, { method: 'POST' });
    await refresh();
  }));
}

document.querySelector('#refreshBtn').addEventListener('click', refresh);
document.querySelector('#logoutBtn').addEventListener('click', () => {
  localStorage.removeItem('adminToken');
  window.location.href = '/login';
});
document.querySelectorAll('.tab-link').forEach(button => button.addEventListener('click', () => {
  activateTab(button.dataset.tab, button.dataset.url).catch(error => alert(error.message));
}));

if (!localStorage.getItem('adminToken')) window.location.href = '/login';
else loadPartial(activeUrl).then(refresh).catch(error => alert(error.message));
