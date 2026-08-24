'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Card } from '@/components/ds/misc';

function Tile({ value, label, sub }) {
  return (
    <Card className="stat-card">
      <div className="stat-value">{value}</div>
      <div className="stat-label">{label}</div>
      {sub && <div className="stat-sub">{sub}</div>}
    </Card>
  );
}

export default function DashboardPage() {
  const [routes, setRoutes] = useState(null);
  const [ospf, setOspf] = useState(null);
  const [bgp, setBgp] = useState(null);

  const refresh = useCallback(() => {
    api('/api/routes?family=v4').then(setRoutes).catch(() => setRoutes(null));
    api('/api/ospf').then(setOspf).catch(() => setOspf(null));
    api('/api/bgp').then(setBgp).catch(() => setBgp(null));
  }, []);
  useEffect(refresh, [refresh]);

  const summary = routes?.summary;
  const ospfFull = (ospf?.state?.neighbors || []).filter((n) => n.state === 'Full').length;
  const ospfTotal = (ospf?.state?.neighbors || []).length;
  const bgpUp = (bgp?.state?.peers || []).filter((p) => p.state === 'Established').length;
  const bgpTotal = (bgp?.state?.peers || []).length;

  return (
    <Shell>
      <div className="page-header"><h2>Dashboard</h2></div>
      <div className="stat-grid">
        <Tile
          value={summary ? summary.routes_v4 + summary.routes_v6 : '—'}
          label="Routes in hardware"
          sub={summary ? `${summary.routes_v4} IPv4 · ${summary.routes_v6} IPv6` : 'syncd unreachable'}
        />
        <Tile
          value={summary ? summary.next_hop_groups : '—'}
          label="ECMP next-hop groups"
          sub={summary ? `${summary.neighbors} neighbors in hardware` : ''}
        />
        <Tile
          value={ospf?.state ? `${ospfFull}/${ospfTotal}` : '—'}
          label="OSPF adjacencies (Full)"
          sub={ospf?.state ? `router-id ${ospf.state.router_id}` : ospf?.config ? 'not running' : 'not configured'}
        />
        <Tile
          value={bgp?.state ? `${bgpUp}/${bgpTotal}` : '—'}
          label="BGP peers established"
          sub={bgp?.state ? `AS ${bgp.state.as_number}` : bgp?.config ? 'not running' : 'not configured'}
        />
      </div>
    </Shell>
  );
}
