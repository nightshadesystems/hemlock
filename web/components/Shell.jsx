'use client';
import { useEffect, useState } from 'react';
import { usePathname, useRouter } from 'next/navigation';
import { api } from '@/lib/api';
import { Header, HeaderAction, Subnav } from '@/components/ds/Header';
import { VerticalNav } from '@/components/ds/VerticalNav';

const TABS = [
  { label: 'Dashboard', href: '/dashboard/' },
  { label: 'Switching', href: '/switching/interfaces/', root: '/switching' },
  { label: 'Routing', href: '/routing/routes/', root: '/routing' },
  { label: 'Security', href: '/security/acls/', root: '/security' },
  { label: 'QoS', href: '/qos/maps/', root: '/qos' },
  { label: 'Services', href: '/services/lldp/', root: '/services' },
  { label: 'System', href: '/system/general/', root: '/system' },
];

// Left-hand navigation per tab.
const SIDE_NAV = {
  '/switching': {
    label: 'Switching',
    items: [
      { id: '/switching/interfaces/', label: 'Interfaces', icon: 'network-settings' },
      { id: '/switching/vlans/', label: 'VLANs', icon: 'network-globe' },
      { id: '/switching/svis/', label: 'SVIs', icon: 'network-switch' },
      { id: '/switching/lag/', label: 'Link Aggregation', icon: 'link' },
      { id: '/switching/spanning-tree/', label: 'Spanning Tree', icon: 'tree-view' },
      { id: '/switching/mac-table/', label: 'MAC Table', icon: 'view-list' },
      { id: '/switching/snooping/', label: 'IGMP/MLD Snooping', icon: 'rss' },
      { id: '/switching/storm-control/', label: 'Storm Control', icon: 'cloud-network' },
      { id: '/switching/mirror/', label: 'Mirroring', icon: 'copy' },
    ],
  },
  '/routing': {
    label: 'Routing',
    items: [
      { id: '/routing/routes/', label: 'Routes', icon: 'map' },
      { id: '/routing/static/', label: 'Static Routes', icon: 'view-list' },
      { id: '/routing/ospf/', label: 'OSPF', icon: 'network-globe' },
      { id: '/routing/bgp/', label: 'BGP', icon: 'cloud-network' },
      { id: '/routing/vrrp/', label: 'VRRP', icon: 'link' },
      { id: '/routing/arp/', label: 'ARP / ND', icon: 'network-settings' },
    ],
  },
  '/security': {
    label: 'Security',
    items: [
      { id: '/security/acls/', label: 'ACLs', icon: 'shield' },
      { id: '/security/copp/', label: 'CoPP', icon: 'firewall' },
      { id: '/security/port-security/', label: 'Port Security', icon: 'lock' },
      { id: '/security/dot1x/', label: '802.1X', icon: 'key' },
      { id: '/security/snooping/', label: 'DHCP Snooping', icon: 'eye' },
    ],
  },
  '/qos': {
    label: 'QoS',
    items: [
      { id: '/qos/maps/', label: 'Maps', icon: 'map' },
      { id: '/qos/wred/', label: 'WRED Profiles', icon: 'filter' },
      { id: '/qos/interfaces/', label: 'Interfaces', icon: 'bar-chart' },
    ],
  },
  '/services': {
    label: 'Services',
    items: [
      { id: '/services/lldp/', label: 'LLDP', icon: 'nodes' },
      { id: '/services/ntp/', label: 'NTP', icon: 'clock' },
      { id: '/services/snmp/', label: 'SNMP', icon: 'bell' },
    ],
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
