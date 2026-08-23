'use client';
import { useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';

export default function StaticRoutesPage() {
  const [routes, setRoutes] = useState(null);
  const [error, setError] = useState(null);

  const refresh = () => {
    api('/api/routes')
      .then((r) => setRoutes(r.static_routes))
      .catch((e) => setError(e.message));
  };
  useEffect(refresh, []);

  return (
    <Shell>
      <div className="page-header">
        <h2>Static Routes</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {!routes && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {routes && (
        <Datagrid
          rowKey={(r) => r.prefix}
          onRefresh={refresh}
          columns={[
            { key: 'prefix', label: 'Prefix', sortable: true, render: (r) => <span className="cell-mono">{r.prefix}</span> },
            { key: 'next_hop', label: 'Next Hop', render: (r) => <span className="cell-mono">{r.next_hop}</span> },
          ]}
          rows={routes}
          placeholder="No static routes configured. Add one with: set routing static <prefix> <next-hop>"
        />
      )}
    </Shell>
  );
}
