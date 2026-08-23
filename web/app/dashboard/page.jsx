'use client';
import { useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, formatUptime, formatSpeed } from '@/lib/api';
import { Card, CardBlock, Alert } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { OperLabel, AdminLabel, ModeLabel } from '@/components/status';

function Stat({ label, value, unit, sub }) {
  return (
    <Card className="stat-card">
      <CardBlock>
        <div className="stat-label">{label}</div>
        <div className="stat-value">
          {value}
          {unit && <span className="unit">{unit}</span>}
        </div>
        {sub && <div className="stat-sub">{sub}</div>}
      </CardBlock>
    </Card>
  );
}

export default function DashboardPage() {
  const [system, setSystem] = useState(null);
  const [interfaces, setInterfaces] = useState(null);
  const [vlans, setVlans] = useState(null);
  const [error, setError] = useState(null);

  useEffect(() => {
    Promise.all([api('/api/system'), api('/api/interfaces'), api('/api/vlans')])
      .then(([s, i, v]) => {
        setSystem(s);
        setInterfaces(i.interfaces);
        setVlans(v.vlans);
      })
      .catch((e) => setError(e.message));
  }, []);

  const ports = (interfaces || []).filter((i) => i.kind === 'ethernet');
  const up = ports.filter((i) => i.oper_up).length;
  const down = ports.filter((i) => !i.oper_up && i.admin_up).length;

  return (
    <Shell>
      <div className="page-header">
        <h2>Dashboard</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {!interfaces && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {interfaces && system && (
        <>
          <div className="stat-grid">
            <Stat
              label="Ports up"
              value={`${up} / ${ports.length}`}
              sub={down > 0 ? `${down} enabled but link down` : 'all enabled ports have link'}
            />
            <Stat label="VLANs" value={vlans ? vlans.length : '—'} sub="including default VLAN 1" />
            <Stat label="Uptime" value={formatUptime(system.uptime_secs)} sub={system.platform_model || system.platform_id} />
            <Stat label="Version" value={`v${system.version}`} sub={`backend ${system.backend}`} />
          </div>
          <h3 className="clr-subsection" style={{ marginBottom: 8 }}>Interface status</h3>
          <Datagrid
            compact
            columns={[
              { key: 'name', label: 'Interface', sortable: true, render: (r) => <span className="cell-mono">{r.name}</span> },
              { key: 'description', label: 'Description', render: (r) => r.description || <span className="dim">—</span> },
              { key: 'mode', label: 'Mode', render: (r) => <ModeLabel iface={r} /> },
              { key: 'admin_up', label: 'Admin', render: (r) => <AdminLabel up={r.admin_up} /> },
              { key: 'oper_up', label: 'Link', sortable: true, render: (r) => <OperLabel up={r.oper_up} /> },
              { key: 'speed_mbps', label: 'Speed', sortable: true, render: (r) => <span className="cell-mono">{formatSpeed(r.speed_mbps)}</span> },
            ]}
            rows={interfaces}
            pageSize={16}
            placeholder="No interfaces reported."
          />
        </>
      )}
    </Shell>
  );
}
