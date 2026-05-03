const isRegister = window.location.pathname === '/register';
const params = new URLSearchParams(window.location.search);
const nextUrl = params.get('next') || '/admin';
const authModes = {
  login: {
    endpoint: '/api/auth/login',
    title: 'Вход в аккаунт',
    subtitle: 'Авторизуйтесь, чтобы продолжить работу с Hromatite.',
    submitText: 'Войти',
  },
  register: {
    endpoint: '/api/auth/register',
    title: 'Создание аккаунта',
    subtitle: 'Создайте пользовательский аккаунт Hromatite.',
    submitText: 'Зарегистрироваться',
  },
};
const mode = isRegister ? authModes.register : authModes.login;
const title = document.querySelector('#pageTitle');
const subtitle = document.querySelector('#pageSubtitle');
const submitBtn = document.querySelector('#submitBtn');
const message = document.querySelector('#authMessage');

document.querySelector('#loginLink').classList.toggle('active', !isRegister);
document.querySelector('#registerLink').classList.toggle('active', isRegister);

title.textContent = mode.title;
subtitle.textContent = mode.subtitle;
submitBtn.textContent = mode.submitText;

document.querySelector('#authForm').addEventListener('submit', async event => {
  event.preventDefault();
  message.textContent = '';
  message.classList.remove('ok');
  const data = Object.fromEntries(new FormData(event.target));
  try {
    const response = await fetch(mode.endpoint, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(data),
    });
    const result = await response.json();
    if (!response.ok) throw new Error(result.detail || 'Ошибка авторизации');
    if (isRegister) {
      message.classList.add('ok');
      message.textContent = 'Аккаунт создан. Теперь войдите.';
      window.setTimeout(() => window.location.href = '/login', 900);
      return;
    }
    localStorage.setItem('adminToken', result.token);
    document.cookie = `adminToken=${encodeURIComponent(result.token)}; Path=/; SameSite=Lax`;
    if (['admin', 'superadmin', 'owner'].includes(result.role)) window.location.href = nextUrl;
    else {
      message.classList.add('ok');
      message.textContent = 'Вход выполнен. Пользовательская панель пока не подключена.';
    }
  } catch (error) {
    message.textContent = error.message;
  }
});
