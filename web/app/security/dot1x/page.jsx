'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, shortName, compareNames, formatUptime } from '@/lib/api';
import { Alert, Badge, Card, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Password, SearchSelect } from '@/components/ds/forms';

/// Add ("Add Server") and edit share one dialog; the address is fixed
/// when editing. The shared secret is write-only: leaving it blank while
/// editing keeps the configured key.
function ServerModal({ open, server, onClose, onSaved }) {
  const editing = !!server;
  const [ip, setIp] = useState('');
  const [key, setKey] = useState('');
  const [port, setPort] = useState('');
  const [timeout_, setTimeout_] = useState('');
  const [retransmit, setRetransmit] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setIp(editing ? server.ip : '');
    setKey('');
    setPort(editing ? server.port || '' : '');
    setTimeout_(editing ? server.timeout || '' : '');
    setRetransmit(editing ? server.retransmit || '' : '');
    setError(null);
    setBusy(false);
  }, [open, editing, server]);

  const submit = async () => {
    const set = { ip: ip.trim() };
    if (key) set.key = key;
    const numeric = [
      ['port', port, 'Port must be 1..65535.', (n) => n >= 1 && n <= 65535],
      ['timeout', timeout_, 'Timeout must be 1..60.', (n) => n >= 1 && n <= 60],
      ['retransmit', retransmit, 'Retransmit must be 0..10.', (n) => n >= 0 && n <= 10],
    ];
    for (const [field, value, message, ok] of numeric) {
      if (!String(value).trim()) continue;
      const parsed = parseInt(value, 10);
      if (!Number.isInteger(parsed) || !ok(parsed)) {
        setError(message);
        return;
      }
      set[field] = parsed;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/dot1x/edit', {
        method: 'POST',
        body: JSON.stringify({ servers_set: [set] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title={editing ? `RADIUS Server ${server.ip}` : 'Add RADIUS Server'}
      size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy || !ip.trim()}>
            Commit
          </Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Address" required htmlFor="radius-ip" helper="IPv4 or IPv6">
          <Input id="radius-ip" className="mono" value={ip} disabled={editing} autoFocus={!editing}
            onChange={(e) => setIp(e.target.value)} style={{ maxWidth: 220 }} />
        </FormField>
        <FormField label="Shared Secret" htmlFor="radius-key"
          helper={editing && server.has_key ? 'Empty keeps the configured key' : 'Required for authentication'}>
          <Password id="radius-key" className="mono" value={key} autoComplete="new-password"
            placeholder={editing && server.has_key ? 'unchanged' : ''}
            onChange={(e) => setKey(e.target.value)} style={{ maxWidth: 220 }} />
        </FormField>
        <FormField label="Port" htmlFor="radius-port" helper="1..65535; empty = 1812">
          <Input id="radius-port" className="mono" value={port}
            onChange={(e) => setPort(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <FormField label="Timeout (s)" htmlFor="radius-timeout" helper="1..60; empty = default">
          <Input id="radius-timeout" className="mono" value={timeout_}
            onChange={(e) => setTimeout_(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <FormField label="Retransmit" htmlFor="radius-retransmit" helper="0..10; empty = default">
          <Input id="radius-retransmit" className="mono" value={retransmit}
            onChange={(e) => setRetransmit(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
      </div>
    </Modal>
  );
}

function ReauthModal({ open, current, onClose, onSaved }) {
  const [interval, setInterval_] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setInterval_(current ? String(current) : '');
    setError(null);
    setBusy(false);
  }, [open, current]);

  const submit = async () => {
    const body = {};
    if (interval.trim()) {
      const parsed = parseInt(interval, 10);
      if (!Number.isInteger(parsed) || (parsed !== 0 && (parsed < 60 || parsed > 86400))) {
        setError('Interval must be 0 (off) or 60..86400 seconds.');
        return;
      }
      body.reauth_interval = parsed;
    } else {
      body.clear_reauth = true;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/dot1x/edit', {
        method: 'POST',
        body: JSON.stringify(body),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title="Reauthentication Interval" size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Interval (s)" htmlFor="dot1x-reauth"
          helper="0 = off; 60..86400; empty reverts to the default">
          <Input id="dot1x-reauth" className="mono" value={interval} autoFocus
            onChange={(e) => setInterval_(e.target.value)} style={{ maxWidth: 160 }} />
        </FormField>
      </div>
    </Modal>
  );
}

function EnablePortModal({ open, interfaces, enabled, onClose, onSaved }) {
  const [port, setPort] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setPort('');
    setError(null);
    setBusy(false);
  }, [open]);

  const options = interfaces
    .filter((name) => !enabled.includes(name))
    .map((name) => ({ value: name, label: name }));

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/dot1x/edit', {
        method: 'POST',
        body: JSON.stringify({ ports_enable: [port] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title="Enable 802.1X on Port" size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy || !port}>
            Commit
          </Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Interface" required helper="Physical ports only">
          <SearchSelect options={options} value={port} onChange={setPort}
            placeholder="Select port…" />
        </FormField>
      </div>
    </Modal>
  );
}

export default function Dot1xPage() {
  const [data, setData] = useState(null);
  const [interfaces, setInterfaces] = useState([]);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api('/api/dot1x')
      .then(setData)
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);
  useEffect(() => {
    api('/api/interfaces')
      .then((r) => setInterfaces(
        r.interfaces
          .map((i) => i.name)
          .filter((n) => n.startsWith('Ethernet'))
          .sort(compareNames)
      ))
      .catch(() => {});
  }, []);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const removeServer = async (ip) => {
    setError(null);
    try {
      const result = await api('/api/dot1x/edit', {
        method: 'POST',
        body: JSON.stringify({ servers_delete: [ip] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  const disablePort = async (port) => {
    setError(null);
    try {
      const result = await api('/api/dot1x/edit', {
        method: 'POST',
        body: JSON.stringify({ ports_disable: [port] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  const reauth = async (port) => {
    setError(null);
    try {
      const result = await api('/api/dot1x/reauth', {
        method: 'POST',
        body: JSON.stringify({ port }),
      });
      setApplied([result.triggered
        ? `Reauthentication triggered on ${port}.`
        : `${port}: no supplicant to reauthenticate.`]);
      refresh();
    } catch (err) {
      setError(err.message);
    }
  };

  return (
    <Shell>
      <div className="page-header">
        <h2>802.1X</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!data && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {data && (
        <>
          {!data.live && (
            <Alert status="warning" style={{ marginBottom: 16 }}>
              Live authenticator state is unavailable; showing configuration only.
            </Alert>
          )}
          <Card
            header={
              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', width: '100%', gap: 16 }}>
                <span style={{ display: 'inline-flex', alignItems: 'center', gap: 12 }}>
                  RADIUS
                  <Label>
                    Reauth {data.reauth_interval ? `${data.reauth_interval}s` : 'off'}
                  </Label>
                </span>
                <span style={{ display: 'inline-flex', gap: 8 }}>
                  <Button variant="outline" sm icon="pencil" onClick={() => setModal({ kind: 'reauth' })}>
                    Reauth Interval
                  </Button>
                  <Button variant="primary" sm icon="plus" onClick={() => setModal({ kind: 'server' })}>
                    Add Server
                  </Button>
                </span>
              </div>
            }
            style={{ marginBottom: 16 }}
          >
            {data.radius_servers.length === 0 && (
              <p className="dim" style={{ margin: 0 }}>
                No RADIUS servers configured — 802.1X ports cannot authorize supplicants.
              </p>
            )}
            {data.radius_servers.map((server) => (
              <div key={server.ip}
                style={{ display: 'flex', alignItems: 'center', gap: 12, padding: '2px 0' }}>
                <span className="cell-mono">
                  {server.ip}:{server.port || 1812}
                </span>
                <span className="dim">
                  {server.has_key ? 'key set' : 'no key'}
                  {server.timeout ? ` · timeout ${server.timeout}s` : ''}
                  {server.retransmit ? ` · retransmit ${server.retransmit}` : ''}
                </span>
                <Button variant="link-neutral" sm icon="pencil" aria-label={`Edit server ${server.ip}`}
                  onClick={() => setModal({ kind: 'server', server })} />
                <Button variant="link-neutral" sm icon="trash" aria-label={`Remove server ${server.ip}`}
                  onClick={() => removeServer(server.ip)} />
              </div>
            ))}
          </Card>

          <Datagrid
            rowKey={(r) => r.port}
            onRefresh={refresh}
            actionBar={() => (
              <Button variant="primary" sm icon="plus" onClick={() => setModal({ kind: 'enable' })}>
                Enable Port
              </Button>
            )}
            columns={[
              {
                key: 'port', label: 'Port', sortable: true,
                compare: (a, b) => compareNames(a.port, b.port),
                render: (r) => <span className="cell-mono">{shortName(r.port)}</span>,
              },
              {
                key: 'status', label: 'State',
                render: (r) => r.status
                  ? (
                    <Label status={r.status === 'authorized' ? 'success' : 'danger'}>
                      {r.status}
                    </Label>
                  )
                  : <span className="dim">—</span>,
              },
              {
                key: 'supplicant_mac', label: 'Supplicant',
                render: (r) => r.supplicant_mac
                  ? <span className="cell-mono">{r.supplicant_mac}</span>
                  : <span className="dim">—</span>,
              },
              {
                key: 'last_auth', label: 'Last Auth',
                render: (r) => r.last_auth_secs_ago != null
                  ? <span>{formatUptime(r.last_auth_secs_ago)} ago</span>
                  : <span className="dim">—</span>,
              },
              {
                key: 'failures', label: 'Failures',
                render: (r) => (
                  <span className={r.failures > 0 ? 'cell-mono' : 'cell-mono dim'}>{r.failures}</span>
                ),
              },
              {
                key: 'actions', label: '', width: 140,
                render: (r) => (
                  <span style={{ display: 'inline-flex', gap: 2, alignItems: 'center' }}>
                    <Button variant="outline" sm disabled={!data.live}
                      onClick={() => reauth(r.port)}>
                      Reauth
                    </Button>
                    <Button variant="link-neutral" sm icon="trash"
                      aria-label={`Disable 802.1X on ${r.port}`}
                      onClick={() => disablePort(r.port)} />
                  </span>
                ),
              },
            ]}
            rows={data.ports}
            placeholder="802.1X is not enabled on any port."
          />
        </>
      )}
      <ServerModal
        open={!!modal && modal.kind === 'server'}
        server={modal && modal.kind === 'server' ? modal.server : null}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
      <ReauthModal
        open={!!modal && modal.kind === 'reauth'}
        current={data ? data.reauth_interval : 0}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
      <EnablePortModal
        open={!!modal && modal.kind === 'enable'}
        interfaces={interfaces}
        enabled={data ? data.ports.map((p) => p.port) : []}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
    </Shell>
  );
}
