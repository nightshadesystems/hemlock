'use client';
import { useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, formatSpeed } from '@/lib/api';
import { Alert } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { OperLabel, AdminLabel, ModeLabel } from '@/components/status';

function vlanSummary(r) {
  if (r.kind !== 'ethernet' || (r.addresses && r.addresses.length > 0)) return '—';
  if (r.switchport_mode === 'trunk') {
    const vlans = (r.trunk_vlans || []).join(',') || '—';
    return `${vlans} · native ${r.native_vlan || 1}`;
  }
  return String(r.access_vlan || 1);
}

export default function InterfacesPage() {
  const [interfaces, setInterfaces] = useState(null);
  const [error, setError] = useState(null);

  useEffect(() => {
    api('/api/interfaces')
      .then((r) => setInterfaces(r.interfaces))
      .catch((e) => setError(e.message));
  }, []);

  return (
    <Shell>
      <div className="page-header">
        <h2>Interfaces</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {!interfaces && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {interfaces && (
        <Datagrid
          columns={[
            { key: 'name', label: 'Interface', sortable: true, render: (r) => <span className="cell-mono">{r.name}</span> },
            { key: 'description', label: 'Description', render: (r) => r.description || <span className="dim">—</span> },
            { key: 'mode', label: 'Mode', render: (r) => <ModeLabel iface={r} /> },
            { key: 'vlans', label: 'VLANs', render: (r) => <span className="cell-mono">{vlanSummary(r)}</span> },
            { key: 'addresses', label: 'Address', render: (r) => (r.addresses && r.addresses.length > 0 ? <span className="cell-mono">{r.addresses.join(', ')}</span> : <span className="dim">—</span>) },
            { key: 'admin_up', label: 'Admin', render: (r) => <AdminLabel up={r.admin_up} /> },
            { key: 'oper_up', label: 'Link', sortable: true, render: (r) => <OperLabel up={r.oper_up} /> },
            { key: 'speed_mbps', label: 'Speed', sortable: true, render: (r) => <span className="cell-mono">{formatSpeed(r.speed_mbps)}</span> },
            { key: 'mtu', label: 'MTU', render: (r) => <span className="cell-mono">{r.mtu || '—'}</span> },
            { key: 'mac', label: 'MAC', render: (r) => <span className="cell-mono">{r.mac || '—'}</span> },
          ]}
          rows={interfaces}
          pageSize={24}
          placeholder="No interfaces reported."
        />
      )}
    </Shell>
  );
}
