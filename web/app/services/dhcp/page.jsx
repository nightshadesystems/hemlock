'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Badge, Card, CardBlock } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Textarea } from '@/components/ds/forms';

const MAX_SERVERS = 4;

const TABS = [
  { id: 'relay', label: 'Relay' },
  { id: 'server', label: 'Server' },
  { id: 'leases', label: 'Leases' },
];

const MAX_DNS_SERVERS = 3;

// A lease expiry as the UTC wall-clock time it falls due — the same
// form `show dhcp server leases` prints.
const expiryClock = (seconds) =>
  seconds == null ? '—' : new Date(seconds * 1000).toISOString().slice(11, 19);

// How long until a lease falls due, for the countdown column.
const remaining = (seconds, now) => {
  if (seconds == null) return null;
  return Math.max(0, seconds - Math.floor(now / 1000));
};

const countdown = (secs) => {
  if (secs == null) return '—';
  if (secs < 60) return `${secs}s`;
  const pad = (n) => String(n).padStart(2, '0');
  if (secs < 3600) return `${Math.floor(secs / 60)}m${pad(secs % 60)}s`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h${pad(Math.floor((secs % 3600) / 60))}m`;
  return `${Math.floor(secs / 86400)}d${pad(Math.floor((secs % 86400) / 3600))}h`;
};

const validServer = (host) => /^(\d{1,3}\.){3}\d{1,3}$/.test(host)
  && host.split('.').every((octet) => Number(octet) <= 255);

/// Add ("Add Relay") and edit share one dialog: the VLAN is fixed when
/// editing, and the whole server list is sent on commit.
function RelayModal({ open, editing, onClose, onSaved }) {
  const [vlan, setVlan] = useState('');
  const [servers, setServers] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setVlan(editing ? String(editing.vlan) : '');
    setServers(editing ? (editing.servers || []).join(', ') : '');
    setError(null);
    setBusy(false);
  }, [open, editing]);

  const submit = async () => {
    const id = parseInt(vlan, 10);
    if (!Number.isInteger(id) || id < 1 || id > 4094) {
      setError('VLAN id must be 1..4094.');
      return;
    }
    const list = servers.split(',').map((s) => s.trim()).filter(Boolean);
    if (list.length === 0) {
      setError('Enter at least one server address.');
      return;
    }
    if (list.length > MAX_SERVERS) {
      setError(`At most ${MAX_SERVERS} servers.`);
      return;
    }
    const bad = list.find((server) => !validServer(server));
    if (bad) {
      setError(`"${bad}" is not an IPv4 address (DHCPv6 relay is not supported).`);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/dhcp/relay/edit', {
        method: 'POST',
        body: JSON.stringify({ set: [{ vlan: id, servers: list }] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title={editing ? `Relay · Vlan${editing.vlan}` : 'Add Relay'} size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy || !vlan}>
            Commit
          </Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="VLAN" required htmlFor="relay-vlan"
          helper="The SVI must carry an address — it becomes the relay's giaddr">
          <Input id="relay-vlan" className="mono" value={vlan} disabled={!!editing}
            onChange={(e) => setVlan(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <FormField label="Servers" required htmlFor="relay-servers"
          helper={`Comma-separated IPv4 addresses, tried in order; up to ${MAX_SERVERS}`}>
          <Input id="relay-servers" className="mono" value={servers}
            onChange={(e) => setServers(e.target.value)} style={{ maxWidth: 'none' }} />
        </FormField>
      </div>
    </Modal>
  );
}

/// The pool editor. Reservations are edited as one text block —
/// `<mac> <ip>` per line — because that is how an operator has them:
/// pasted out of an inventory, not typed one field at a time.
function PoolModal({ open, editing, onClose, onSaved }) {
  const [name, setName] = useState('');
  const [network, setNetwork] = useState('');
  const [rangeStart, setRangeStart] = useState('');
  const [rangeEnd, setRangeEnd] = useState('');
  const [gateway, setGateway] = useState('');
  const [dns, setDns] = useState('');
  const [lease, setLease] = useState('');
  const [domain, setDomain] = useState('');
  const [reservations, setReservations] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setName(editing ? editing.name : '');
    setNetwork(editing ? editing.network || '' : '');
    setRangeStart(editing ? editing.range_start || '' : '');
    setRangeEnd(editing ? editing.range_end || '' : '');
    setGateway(editing ? editing.gateway || '' : '');
    setDns(editing ? (editing.dns_servers || []).join(', ') : '');
    setLease(editing && editing.lease_time ? String(editing.lease_time) : '');
    setDomain(editing ? editing.domain_name || '' : '');
    setReservations(
      editing
        ? (editing.reservations || []).map((r) => `${r.mac} ${r.address}`).join('\n')
        : '',
    );
    setError(null);
    setBusy(false);
  }, [open, editing]);

  const submit = async () => {
    if (!/^[A-Za-z][A-Za-z0-9_-]{0,31}$/.test(name)) {
      setError('A pool name starts with a letter (letters, digits, _ or -; max 32).');
      return;
    }
    const dnsList = dns.split(',').map((s) => s.trim()).filter(Boolean);
    if (dnsList.length > MAX_DNS_SERVERS) {
      setError(`At most ${MAX_DNS_SERVERS} DNS servers.`);
      return;
    }
    const parsedReservations = [];
    for (const line of reservations.split('\n').map((l) => l.trim()).filter(Boolean)) {
      const [mac, address] = line.split(/\s+/);
      if (!mac || !address) {
        setError(`Each reservation is "<mac> <ip>"; got "${line}".`);
        return;
      }
      parsedReservations.push({ mac, address });
    }
    const set = { name, dns_servers: dnsList, reservations: parsedReservations };
    if (network) set.network = network;
    if (rangeStart || rangeEnd) {
      set.range_start = rangeStart;
      set.range_end = rangeEnd;
    }
    if (gateway) set.default_gateway = gateway;
    set.lease_time = lease ? parseInt(lease, 10) : 0;
    set.domain_name = domain;
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/dhcp/server/edit', {
        method: 'POST',
        body: JSON.stringify({ set: [set] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title={editing ? `Pool · ${editing.name}` : 'Add Pool'}
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy || !name}>
            Commit
          </Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Pool" required htmlFor="pool-name">
          <Input id="pool-name" className="mono" value={name} disabled={!!editing}
            onChange={(e) => setName(e.target.value)} />
        </FormField>
        <FormField label="Network" required htmlFor="pool-network" helper="e.g. 10.0.10.0/24">
          <Input id="pool-network" className="mono" value={network}
            onChange={(e) => setNetwork(e.target.value)} />
        </FormField>
        <FormField label="Range Start" required htmlFor="pool-range-start">
          <Input id="pool-range-start" className="mono" value={rangeStart}
            onChange={(e) => setRangeStart(e.target.value)} />
        </FormField>
        <FormField label="Range End" required htmlFor="pool-range-end">
          <Input id="pool-range-end" className="mono" value={rangeEnd}
            onChange={(e) => setRangeEnd(e.target.value)} />
        </FormField>
        <FormField label="Default Gateway" required htmlFor="pool-gateway">
          <Input id="pool-gateway" className="mono" value={gateway}
            onChange={(e) => setGateway(e.target.value)} />
        </FormField>
        <FormField label="DNS Servers" htmlFor="pool-dns"
          helper={`Comma-separated; up to ${MAX_DNS_SERVERS}`}>
          <Input id="pool-dns" className="mono" value={dns}
            onChange={(e) => setDns(e.target.value)} />
        </FormField>
        <FormField label="Lease Time" htmlFor="pool-lease"
          helper="Seconds, 300..2592000; empty uses the default 86400">
          <Input id="pool-lease" className="mono" value={lease}
            onChange={(e) => setLease(e.target.value)} style={{ maxWidth: 140 }} />
        </FormField>
        <FormField label="Domain Name" htmlFor="pool-domain" helper="Empty clears it">
          <Input id="pool-domain" className="mono" value={domain}
            onChange={(e) => setDomain(e.target.value)} />
        </FormField>
        <FormField label="Reservations" htmlFor="pool-reservations"
          helper="One per line: <mac> <ip>">
          <Textarea id="pool-reservations" className="mono" rows={4} value={reservations}
            onChange={(e) => setReservations(e.target.value)} />
        </FormField>
      </div>
    </Modal>
  );
}

export default function DhcpPage() {
  const [tab, setTab] = useState('relay');
  const [state, setState] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);
  const [now, setNow] = useState(() => Date.now());

  const refresh = useCallback(() => {
    api('/api/dhcp')
      .then(setState)
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  // The relay counters and lease table move while clients renew.
  useEffect(() => {
    const id = setInterval(refresh, 10_000);
    return () => clearInterval(id);
  }, [refresh]);
  // ...and the expiry countdown ticks between refreshes.
  useEffect(() => {
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, []);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const removeRelay = async (vlan) => {
    try {
      const result = await api('/api/dhcp/relay/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: [vlan] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  const relays = state ? state.relay : [];
  const pools = state ? state.pools || [] : [];
  const leases = state ? state.leases || [] : [];

  const removePool = async (name) => {
    try {
      const result = await api('/api/dhcp/server/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: [name] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  const clearLease = async (address) => {
    try {
      await api('/api/dhcp/leases/clear', {
        method: 'POST',
        body: JSON.stringify({ address }),
      });
      setApplied([`lease ${address} cleared`]);
      refresh();
    } catch (err) {
      setError(err.message);
    }
  };

  return (
    <Shell>
      <div className="page-header">
        <h2>DHCP</h2>
        <span style={{ display: 'inline-flex', gap: 4 }}>
          {TABS.map((t) => (
            <Button key={t.id} variant={tab === t.id ? 'primary' : 'outline'} sm
              onClick={() => setTab(t.id)}>
              {t.label}
            </Button>
          ))}
        </span>
        {tab === 'relay' && (
          <Button variant="primary" sm icon="plus" onClick={() => setModal({ kind: 'relay' })}>
            Add Relay
          </Button>
        )}
        {tab === 'server' && (
          <Button variant="primary" sm icon="plus" onClick={() => setModal({ kind: 'pool' })}>
            Add Pool
          </Button>
        )}
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!state && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {state && tab === 'relay' && (
        <Datagrid
          rowKey={(r) => r.vlan}
          onRefresh={refresh}
          columns={[
            {
              key: 'vlan', label: 'VLAN', sortable: true,
              render: (r) => <span className="cell-mono">{r.vlan}</span>,
            },
            {
              key: 'servers', label: 'Servers',
              render: (r) => <span className="cell-mono">{(r.servers || []).join(', ')}</span>,
            },
            {
              key: 'giaddr', label: 'Relay Address',
              render: (r) => r.giaddr
                ? <span className="cell-mono">{r.giaddr}</span>
                : <Badge status="warning">no address</Badge>,
            },
            {
              key: 'to_server', label: 'To Server',
              render: (r) => <span className="cell-mono">{r.to_server}</span>,
            },
            {
              key: 'to_client', label: 'To Client',
              render: (r) => <span className="cell-mono">{r.to_client}</span>,
            },
            {
              key: 'dropped', label: 'Dropped',
              render: (r) => r.dropped
                ? <Badge status="warning">{r.dropped}</Badge>
                : <span className="cell-mono">0</span>,
            },
            {
              key: 'actions', label: '', width: 80,
              render: (r) => (
                <span style={{ display: 'inline-flex', gap: 2 }}>
                  <Button variant="link-neutral" sm icon="pencil"
                    aria-label={`Edit Vlan${r.vlan}`}
                    onClick={() => setModal({ editing: r })} />
                  <Button variant="link-neutral" sm icon="trash"
                    aria-label={`Remove the relay on Vlan${r.vlan}`}
                    onClick={() => removeRelay(r.vlan)} />
                </span>
              ),
            },
          ]}
          rows={relays}
          placeholder="No DHCP relays configured."
        />
      )}
      {state && tab === 'server' && (
        <>
          <div className="card-grid" style={{ marginBottom: 16 }}>
            {pools.map((pool) => {
              const used = pool.capacity ? Math.round((pool.in_use / pool.capacity) * 100) : 0;
              return (
                <Card key={pool.name}
                  header={
                    <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', width: '100%', gap: 16 }}>
                      <span className="mono">{pool.name}</span>
                      <span style={{ display: 'inline-flex', gap: 2 }}>
                        <Button variant="link-neutral" sm icon="pencil"
                          aria-label={`Edit ${pool.name}`}
                          onClick={() => setModal({ kind: 'pool', editing: pool })} />
                        <Button variant="link-neutral" sm icon="trash"
                          aria-label={`Remove ${pool.name}`}
                          onClick={() => removePool(pool.name)} />
                      </span>
                    </div>
                  }>
                  <CardBlock title="Network" text={pool.network} />
                  <CardBlock title="Range" text={`${pool.range_start} - ${pool.range_end}`} />
                  <CardBlock title="Gateway" text={pool.gateway} />
                  {(pool.dns_servers || []).length > 0 && (
                    <CardBlock title="DNS" text={pool.dns_servers.join(', ')} />
                  )}
                  <CardBlock title="Lease" text={`${pool.lease_time} s`} />
                  <div className="card-block">
                    <div className="card-title">
                      Utilisation — {pool.in_use} / {pool.capacity}
                    </div>
                    <div className="progress" style={{ marginTop: 6 }}
                      role="progressbar" aria-valuenow={used} aria-valuemin={0}
                      aria-valuemax={100}
                      aria-label={`${pool.name} utilisation`}>
                      <span className="progress-fill" style={{ width: `${used}%` }}></span>
                    </div>
                  </div>
                  {(pool.reservations || []).length > 0 && (
                    <CardBlock title="Reservations"
                      text={`${pool.reservations.length} configured`} />
                  )}
                </Card>
              );
            })}
          </div>
          {pools.length === 0 && (
            <Alert status="info">
              No pools configured; the DHCP server is off.
            </Alert>
          )}
        </>
      )}
      {state && tab === 'leases' && (
        <Datagrid
          rowKey={(r) => `${r.address}-${r.mac}`}
          onRefresh={refresh}
          columns={[
            {
              key: 'address', label: 'IP Address', sortable: true,
              render: (r) => <span className="cell-mono">{r.address}</span>,
            },
            {
              key: 'mac', label: 'MAC Address',
              render: (r) => <span className="cell-mono">{r.mac}</span>,
            },
            {
              key: 'hostname', label: 'Hostname',
              render: (r) => r.hostname
                ? <span className="cell-mono">{r.hostname}</span>
                : <span className="dim">—</span>,
            },
            {
              key: 'pool', label: 'Pool',
              render: (r) => r.pool
                ? <span className="cell-mono">{r.pool}</span>
                : <span className="dim">—</span>,
            },
            {
              key: 'expires_at', label: 'Expires',
              render: (r) => r.expires_at == null
                ? <span className="dim">—</span>
                : (
                  <span className="cell-mono" title={expiryClock(r.expires_at)}>
                    {countdown(remaining(r.expires_at, now))}
                  </span>
                ),
            },
            {
              key: 'reservation', label: 'Type',
              render: (r) => r.reservation
                ? <Badge status="info">reservation</Badge>
                : <Badge>dynamic</Badge>,
            },
            {
              key: 'actions', label: '', width: 60,
              render: (r) => r.expires_at == null ? null : (
                <Button variant="link-neutral" sm icon="trash"
                  aria-label={`Clear the lease for ${r.address}`}
                  onClick={() => clearLease(r.address)} />
              ),
            },
          ]}
          rows={leases}
          placeholder="No leases issued."
        />
      )}
      <RelayModal open={!!modal && modal.kind === 'relay'}
        editing={modal && modal.kind === 'relay' ? modal.editing : null}
        onClose={() => setModal(null)} onSaved={onSaved} />
      <PoolModal open={!!modal && modal.kind === 'pool'}
        editing={modal && modal.kind === 'pool' ? modal.editing : null}
        onClose={() => setModal(null)} onSaved={onSaved} />
    </Shell>
  );
}
