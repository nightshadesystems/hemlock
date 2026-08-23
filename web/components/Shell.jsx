'use client';
import { useEffect, useState } from 'react';
import { usePathname, useRouter } from 'next/navigation';
import { api } from '@/lib/api';
import { Header, HeaderAction, Subnav } from '@/components/ds/Header';
import { VerticalNav } from '@/components/ds/VerticalNav';

const TABS = [
  { label: 'Dashboard', href: '/dashboard/' },
  { label: 'Switching', href: '/switching/interfaces/', root: '/switching' },
  { label: 'Routing', href: '/routing/static/', root: '/routing' },
  { label: 'System', href: '/system/general/', root: '/system' },
];

// Left-hand navigation per tab.
const SIDE_NAV = {
  '/switching': {
    label: 'Switching',
    items: [
      { id: '/switching/interfaces/', label: 'Interfaces', icon: 'network-settings' },
      { id: '/switching/vlans/', label: 'VLANs', icon: 'network-globe' },
    ],
  },
  '/routing': {
    label: 'Routing',
    items: [{ id: '/routing/static/', label: 'Static routes', icon: 'map' }],
  },
  '/system': {
    label: 'System',
    items: [
      { id: '/system/general/', label: 'General', icon: 'cog' },
      { id: '/system/users/', label: 'Users', icon: 'users' },
      { id: '/system/maintenance/', label: 'Maintenance', icon: 'wrench' },
    ],
  },
};

function useTheme() {
  const [theme, setTheme] = useState('dark');
  useEffect(() => {
    setTheme(localStorage.getItem('hemlock-theme') || 'dark');
  }, []);
  const toggle = () => {
    const next = theme === 'dark' ? 'light' : 'dark';
    setTheme(next);
    document.documentElement.setAttribute('data-theme', next);
    localStorage.setItem('hemlock-theme', next);
  };
  return [theme, toggle];
}

export default function Shell({ children }) {
  const router = useRouter();
  const pathname = usePathname() || '/';
  const [session, setSession] = useState(null);
  const [hostname, setHostname] = useState('');
  const [theme, toggleTheme] = useTheme();

  useEffect(() => {
    api('/api/session').then(setSession).catch(() => {});
    api('/api/system').then((s) => setHostname(s.hostname)).catch(() => {});
  }, []);

  const logout = async () => {
    try {
      await api('/api/logout', { method: 'POST' });
    } finally {
      router.replace('/login/');
    }
  };

  const activeTab = TABS.find((t) => pathname.startsWith(t.root || t.href));
  const sideKey = Object.keys(SIDE_NAV).find((k) => pathname.startsWith(k));
  const side = sideKey ? SIDE_NAV[sideKey] : null;

  if (!session) {
    return <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>;
  }

  return (
    <div className="main-container">
      <Header logo="/brand/nightshade-symbol-on-dark.svg" title="Hemlock"
        actions={
          <>
            <HeaderAction icon={theme === 'dark' ? 'sun' : 'moon'} label="Toggle theme" onClick={toggleTheme} />
            <HeaderAction icon="logout" label={`Sign out ${session.username}`} title={`Sign out (${session.username})`} onClick={logout} />
          </>
        }
      >
        <div className="header-dropdown">
          <button style={{ cursor: 'default' }} tabIndex={-1}>
            <span className="hd-text">
              <span className="hd-label">Switch</span>
              <span className="hd-value mono">{hostname || '—'}</span>
            </span>
          </button>
        </div>
        <span className="header-divider"></span>
      </Header>
      <Subnav
        items={TABS.map((t) => ({ ...t, active: t === activeTab }))}
        onNavigate={(t) => router.push(t.href)}
      />
      <div className="content-container">
        {side && (
          <VerticalNav
            collapsible
            activeId={pathname}
            groups={[side]}
            onNavigate={(it) => router.push(it.id)}
          />
        )}
        <main className="content-area">{children}</main>
      </div>
    </div>
  );
}
