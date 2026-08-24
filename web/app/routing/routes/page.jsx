'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, formatUptime } from '@/lib/api';
import { Alert, Card, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button, ButtonGroup } from '@/components/ds/Button';

const PROTOCOL_LABEL = {
  connected: 'C',
  static: 'S',
  kernel: 'K',
  ospf: 'O',
  bgp: 'B',
};

function FibBadge({ fib }) {
  const status =
    fib === 'programmed' || fib === 'connected' || fib === 'drop'
      ? 'success'
      : fib === 'punt'
        ? 'warning'
        : 'info';
  return <Label status={status}>{fib}</Label>;
}

function NextHops({ route }) {
  if (route.interface) {
    return <span className="cell-mono">directly connected, {route.interface}</span>;
  }
  if (!route.next_hops || route.next_hops.length === 0) {
    return <span className="dim">—</span>;
  }
  return (
    <span style={{ display: 'inline-flex', flexDirection: 'column', gap: 2 }}>
      {route.next_hops.map((hop) => (
        <span key={`${hop.via}-${hop.interface}`} className="cell-mono"
          style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}>
          via {hop.via}
          {hop.interface ? `, ${hop.interface}` : ''}
          {!hop.resolved && <Label status="warning">unresolved</Label>}
        </span>
      ))}
    </span>
  );
}

export default function RoutesPage() {
  const [family, setFamily] = useState('v4');
  const [data, setData] = useState(null);
  const [error, setError] = useState(null);

  const refresh = useCallback(() => {
    api(`/api/routes?family=${family}`)
      .then((r) => {
        setData(r);
        setError(null);
      })
      .catch((e) => setError(e.message));
  }, [family]);
  useEffect(refresh, [refresh]);

  const summary = data?.summary;
  const rib = data?.rib || [];

  return (
    <Shell>
      <div className="page-header" style={{ display: 'flex', alignItems: 'center', gap: 16 }}>
        <h2 style={{ marginRight: 'auto' }}>Routes</h2>
        <ButtonGroup>
          <Button sm variant={family === 'v4' ? 'primary' : 'outline'}
            onClick={() => setFamily('v4')}>IPv4</Button>
          <Button sm variant={family === 'v6' ? 'primary' : 'outline'}
            onClick={() => setFamily('v6')}>IPv6</Button>
        </ButtonGroup>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {summary && (
        <div className="stat-grid" style={{ marginBottom: 16 }}>
          <Card className="stat-card">
            <div className="stat-value">{summary.routes_v4}</div>
            <div className="stat-label">IPv4 routes in hardware</div>
          </Card>
          <Card className="stat-card">
            <div className="stat-value">{summary.routes_v6}</div>
            <div className="stat-label">IPv6 routes in hardware</div>
          </Card>
          <Card className="stat-card">
            <div className="stat-value">{summary.neighbors}</div>
            <div className="stat-label">Neighbors in hardware</div>
          </Card>
          <Card className="stat-card">
            <div className="stat-value">{summary.next_hop_groups}</div>
            <div className="stat-label">ECMP next-hop groups</div>
          </Card>
        </div>
      )}
      {!data && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {data && (
        <Datagrid
          rowKey={(r) => r.prefix}
          onRefresh={refresh}
          columns={[
            {
              key: 'protocol', label: 'Proto', width: 70,
              render: (r) => (
                <span className="cell-mono" title={r.protocol}>
                  {PROTOCOL_LABEL[r.protocol] || '?'}
                </span>
              ),
            },
            {
              key: 'prefix', label: 'Prefix', sortable: true,
              render: (r) => <span className="cell-mono">{r.prefix}</span>,
            },
            {
              key: 'dm', label: 'Dist/Metric', width: 110,
              render: (r) =>
                r.protocol === 'connected'
                  ? <span className="dim">—</span>
                  : <span className="cell-mono">[{r.distance}/{r.metric}]</span>,
            },
            { key: 'next_hops', label: 'Next Hops', render: (r) => <NextHops route={r} /> },
            { key: 'fib', label: 'FIB', width: 110, render: (r) => <FibBadge fib={r.fib} /> },
            {
              key: 'uptime', label: 'Uptime', width: 100,
              render: (r) => <span className="cell-mono">{formatUptime(r.uptime_secs)}</span>,
            },
          ]}
          rows={rib}
          placeholder="No routes reported by the RIB pipeline (is hemlock-orch running with a kernel feed?)."
        />
      )}
    </Shell>
  );
}
