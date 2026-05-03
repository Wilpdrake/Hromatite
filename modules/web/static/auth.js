const isRegister = window.location.pathname === '/register';
const title = document.querySelector('#pageTitle');
const subtitle = document.querySelector('#pageSubtitle');
const submitBtn = document.querySelector('#submitBtn');
const message = document.querySelector('#authMessage');

document.querySelector('#loginLink').classList.toggle('active', !isRegister);
document.querySelector('#registerLink').classList.toggle('active', isRegister);

title.textContent = isRegister ? 'Создание аккаунта' : 'Вход в аккаунт';
subtitle.textContent = isRegister ? 'Создайте пользовательский аккаунт Hromatite.' : 'Авторизуйтесь, чтобы продолжить работу с Hromatite.';
submitBtn.textContent = isRegister ? 'Зарегистрироваться' : 'Войти';

document.querySelector('#authForm').addEventListener('submit', async event => {
  event.preventDefault();
  message.textContent = '';
  message.classList.remove('ok');
  const data = Object.fromEntries(new FormData(event.target));
  const endpoint = isRegister ? '/api/auth/register' : '/api/auth/login';
  try {
    const response = await fetch(endpoint, {
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
    if (['admin', 'superadmin', 'owner'].includes(result.role)) window.location.href = '/admin';
    else {
      message.classList.add('ok');
      message.textContent = 'Вход выполнен. Пользовательская панель пока не подключена.';
    }
  } catch (error) {
    message.textContent = error.message;
  }
});
