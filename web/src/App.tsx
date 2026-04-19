import { BrowserRouter, Routes, Route, NavLink } from 'react-router-dom';
import Dashboard from './pages/Dashboard';
import Sessions from './pages/Sessions';
import Status from './pages/Status';

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
      </nav>
      <main className="content">
        <Routes>
          <Route path="/" element={<Dashboard />} />
          <Route path="/sessions" element={<Sessions />} />
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
