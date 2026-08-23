'use client';
import { useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';

function PortList({ ports }) {
  if (!ports || ports.length === 0) return <span className="dim">—</span>;
  return <span className="cell-mono">{ports.join(', ')}</span>;
}

export default function VlansPage() {
  const [vlans, setVlans] = useState(null);
  const [error, setError] = useState(null);

  useEffect(() => {
    api('/api/vlans')
      .then((r) => setVlans(r.vlans))
      .catch((e) => setError(e.message));
  }, []);

  return (
    <Shell>
      <div className="page-header">
        <h2>VLANs</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {!vlans && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {vlans && (
        <Datagrid
          columns={[
            { key: 'id', label: 'VLAN', sortable: true, render: (r) => <span className="cell-mono">{r.id}</span> },
            { key: 'name', label: 'Name', render: (r) => r.name || <span className="dim">—</span> },
            {
              key: 'svi',
              label: 'SVI',
              render: (r) =>
                r.svi ? (
                  <span className="cell-mono">
                    {r.svi.name}
                    {r.svi.address ? ` · ${r.svi.address}` : ''}
                  </span>
                ) : (
                  <span className="dim">—</span>
                ),
            },
            { key: 'untagged', label: 'Untagged ports', render: (r) => <PortList ports={r.untagged} /> },
            { key: 'tagged', label: 'Tagged ports', render: (r) => <PortList ports={r.tagged} /> },
            {
              key: 'default',
              label: '',
              width: 90,
              render: (r) => (r.id === 1 ? <Label>default</Label> : null),
            },
          ]}
          rows={vlans}
          placeholder="No VLANs configured. Create one with: set vlans vlan <id>"
        />
      )}
    </Shell>
  );
}
