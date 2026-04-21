import { useState, useEffect } from 'react';
import { BrowserRouter, Routes, Route, NavLink } from 'react-router-dom';
import Dashboard from './pages/Dashboard';
import Sessions from './pages/Sessions';
import SessionDetail from './pages/SessionDetail';
import Plugins from './pages/Plugins';
import Settings from './pages/Settings';
import Status from './pages/Status';
import NewChatButton from './components/NewChatButton';

function ThemeToggle() {
  const [theme, setTheme] = useState(() => {
    return localStorage.getItem('hermes-theme') || 'dark';
  });

  useEffect(() => {
    document.documentElement.setAttribute('data-theme', theme);
    localStorage.setItem('hermes-theme', theme);
  }, [theme]);

  const toggle = () => setTheme(t => t === 'dark' ? 'light' : 'dark');

  return (
    <button className="theme-toggle" onClick={toggle} title={theme === 'dark' ? 'Switch to light' : 'Switch to dark'}>
      {theme === 'dark' ? (
        <svg viewBox="0 0 16 16" width="16" height="16" fill="currentColor">
          <path d="M8 11a3 3 0 110-6 3 3 0 010 6zm0 1a4 4 0 100-8 4 4 0 000 8zM8 0a.5.5 0 01.5.5v2a.5.5 0 01-1 0v-2A.5.5 0 018 0zm0 13a.5.5 0 01.5.5v2a.5.5 0 01-1 0v-2A.5.5 0 018 13zm8-5a.5.5 0 01-.5.5h-2a.5.5 0 010-1h2a.5.5 0 01.5.5zM3 8a.5.5 0 01-.5.5h-2a.5.5 0 010-1h2A.5.5 0 013 8zm10.657-5.657a.5.5 0 010 .707l-1.414 1.415a.5.5 0 11-.707-.708l1.414-1.414a.5.5 0 01.707 0zm-9.193 9.193a.5.5 0 010 .707L3.05 13.657a.5.5 0 01-.707-.707l1.414-1.414a.5.5 0 01.707 0zm9.193 2.121a.5.5 0 01-.707 0l-1.414-1.414a.5.5 0 01.707-.707l1.414 1.414a.5.5 0 010 .707zM4.464 4.465a.5.5 0 01-.707 0L2.343 3.05a.5.5 0 11.707-.707l1.414 1.414a.5.5 0 010 .708z" />
        </svg>
      ) : (
        <svg viewBox="0 0 16 16" width="16" height="16" fill="currentColor">
          <path d="M6 .278a.768.768 0 01.08.858 7.208 7.208 0 00-.878 3.46c0 4.021 3.278 7.277 7.318 7.277.527 0 1.04-.055 1.533-.16a.787.787 0 01.81.316.733.733 0 01-.031.893A8.349 8.349 0 018.344 16C3.734 16 0 12.286 0 7.71 0 4.266 2.114 1.312 5.124.06A.752.752 0 016 .278z" />
        </svg>
      )}
    </button>
  );
}

function Layout() {
  return (
    <div className="app">
      <nav className="sidebar">
        <div className="logo">◆ Hermes</div>
        <NavLink to="/" end className={({ isActive }) => isActive ? 'active' : ''}>
          Dashboard
        </NavLink>
        <NavLink to="/sessions" className={({ isActive }) => isActive ? 'active' : ''}>
          Sessions
        </NavLink>
        <NavLink to="/status" className={({ isActive }) => isActive ? 'active' : ''}>
          Status
        </NavLink>
        <NavLink to="/plugins" className={({ isActive }) => isActive ? 'active' : ''}>
          Plugins
        </NavLink>
        <NavLink to="/settings" className={({ isActive }) => isActive ? 'active' : ''}>
          Settings
        </NavLink>
        <NewChatButton />
        <div className="sidebar-footer">
          <ThemeToggle />
        </div>
      </nav>
      <main className="content">
        <Routes>
          <Route path="/" element={<Dashboard />} />
          <Route path="/sessions" element={<Sessions />} />
          <Route path="/sessions/:id" element={<SessionDetail />} />
          <Route path="/plugins" element={<Plugins />} />
          <Route path="/settings" element={<Settings />} />
          <Route path="/status" element={<Status />} />
        </Routes>
      </main>
    </div>
  );
}

export default function App() {
  return (
    <BrowserRouter>
      <Layout />
    </BrowserRouter>
  );
}
