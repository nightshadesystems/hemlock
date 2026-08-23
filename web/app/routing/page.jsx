'use client';
import { useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';

export default function RoutingPage() {
  const [routes, setRoutes] = useState(null);
  const [error, setError] = useState(null);

  useEffect(() => {
    api('/api/routes')
      .then((r) => setRoutes(r.static_routes))
      .catch((e) => setError(e.message));
  }, []);

  return (
    <Shell>
      <div className="page-header">
        <h2>Routing</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {!routes && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {routes && (
        <>
          <h3 className="clr-subsection" style={{ marginBottom: 8 }}>Static routes</h3>
          <Datagrid
            columns={[
              { key: 'prefix', label: 'Prefix', sortable: true, render: (r) => <span className="cell-mono">{r.prefix}</span> },
              { key: 'next_hop', label: 'Next hop', render: (r) => <span className="cell-mono">{r.next_hop}</span> },
            ]}
            rows={routes}
            placeholder="No static routes configured. Add one with: set routing static <prefix> <next-hop>"
          />
        </>
      )}
    </Shell>
  );
}
