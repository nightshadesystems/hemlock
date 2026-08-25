'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Badge } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input } from '@/components/ds/forms';

const MAX_SERVERS = 4;

// The tabs this page will grow; the DHCP server and its leases land
// with the dnsmasq-backed pools.
const TABS = [{ id: 'relay', label: 'Relay' }];

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

export default function DhcpPage() {
  const [tab, setTab] = useState('relay');
  const [state, setState] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api('/api/dhcp')
      .then(setState)
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  // The relay counters move while clients renew.
  useEffect(() => {
    const id = setInterval(refresh, 10_000);
    return () => clearInterval(id);
  }, [refresh]);

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
          <Button variant="primary" sm icon="plus" onClick={() => setModal({})}>
            Add Relay
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
      <RelayModal open={!!modal} editing={modal && modal.editing}
        onClose={() => setModal(null)} onSaved={onSaved} />
    </Shell>
  );
}
