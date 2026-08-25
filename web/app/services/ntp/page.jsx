'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Badge, Card, CardBlock } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input } from '@/components/ds/forms';

const MAX_SERVERS = 4;

// Microseconds as milliseconds to three places — the resolution
// timesyncd reports at, and what `show ntp` prints.
const millis = (usecs) => {
  const sign = usecs < 0 ? '-' : '';
  const magnitude = Math.abs(usecs || 0);
  return `${sign}${Math.floor(magnitude / 1000)}.${String(magnitude % 1000).padStart(3, '0')} ms`;
};

// A short age: 41s, 4m12s, 2h04m, 3d05h.
const age = (secs) => {
  if (secs == null) return 'unknown';
  if (secs < 60) return `${secs}s`;
  const pad = (n) => String(n).padStart(2, '0');
  if (secs < 3600) return `${Math.floor(secs / 60)}m${pad(secs % 60)}s`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h${pad(Math.floor((secs % 3600) / 60))}m`;
  return `${Math.floor(secs / 86400)}d${pad(Math.floor((secs % 86400) / 3600))}h`;
};

// An IP literal or a syntactically valid hostname — the same rule the
// CLI checks at the prompt; mgmtd re-validates on commit.
const validServer = (host) => {
  if (!host) return false;
  if (/^[0-9.]+$/.test(host) || host.includes(':')) return true;
  const name = host.endsWith('.') ? host.slice(0, -1) : host;
  if (!name || name.length > 253) return false;
  return name
    .split('.')
    .every((label) => /^[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?$/.test(label) && label.length <= 63);
};

/// Add ("Add Server") and edit share one dialog; the whole list is
/// sent on commit, because the config leaf is a set.
function ServerModal({ open, servers, editing, onClose, onSaved }) {
  const [host, setHost] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setHost(editing || '');
    setError(null);
    setBusy(false);
  }, [open, editing]);

  const submit = async () => {
    const value = host.trim();
    if (!validServer(value)) {
      setError('Enter an IP address or a hostname.');
      return;
    }
    const next = editing
      ? servers.map((s) => (s === editing ? value : s))
      : [...servers, value];
    const deduped = [...new Set(next)];
    if (deduped.length > MAX_SERVERS) {
      setError(`At most ${MAX_SERVERS} servers.`);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/ntp/edit', {
        method: 'POST',
        body: JSON.stringify({ servers: deduped }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title={editing ? `NTP Server · ${editing}` : 'Add NTP Server'} size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy || !host}>
            Commit
          </Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Server" required htmlFor="ntp-server"
          helper={`IP address or hostname; up to ${MAX_SERVERS} servers`}>
          <Input id="ntp-server" className="mono" value={host}
            onChange={(e) => setHost(e.target.value)} />
        </FormField>
      </div>
    </Modal>
  );
}

export default function NtpPage() {
  const [state, setState] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api('/api/ntp')
      .then(setState)
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  // Sync state moves on its own; poll it while the page is open.
  useEffect(() => {
    const id = setInterval(refresh, 10_000);
    return () => clearInterval(id);
  }, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const removeServer = async (host) => {
    try {
      const result = await api('/api/ntp/edit', {
        method: 'POST',
        body: JSON.stringify({ servers: state.servers.filter((s) => s !== host) }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  const servers = state ? state.servers : [];
  const rows = servers.map((host) => ({ host, active: host === state.server }));

  return (
    <Shell>
      <div className="page-header">
        <h2>NTP</h2>
        <Button variant="primary" sm icon="plus"
          disabled={servers.length >= MAX_SERVERS}
          onClick={() => setModal({ kind: 'server' })}>
          Add Server
        </Button>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!state && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {state && (
        <>
          <Card
            header={
              <span style={{ display: 'inline-flex', alignItems: 'center', gap: 12 }}>
                Clock
                <Badge status={state.synchronized ? 'success' : servers.length ? 'warning' : 'danger'}>
                  {state.synchronized
                    ? 'Synchronized'
                    : servers.length
                      ? 'Not synchronized'
                      : 'Disabled'}
                </Badge>
                {servers.length > 0 && !state.enabled && (
                  <Badge status="warning">timesyncd not running</Badge>
                )}
              </span>
            }
            style={{ marginBottom: 16 }}
          >
            {state.synchronized ? (
              <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 12 }}>
                <CardBlock title="Server" text={state.server || '—'} />
                <CardBlock title="Stratum" text={String(state.stratum)} />
                <CardBlock title="Poll Interval" text={`${state.poll_interval_secs} s`} />
                <CardBlock title="Offset" text={millis(state.offset_usecs)} />
                <CardBlock title="Delay" text={millis(state.delay_usecs)} />
                <CardBlock title="Jitter" text={millis(state.jitter_usecs)} />
                <CardBlock title="Last Sync" text={`${age(state.last_sync_secs_ago)} ago`} />
              </div>
            ) : (
              <CardBlock
                title="Status"
                text={
                  servers.length
                    ? 'Waiting for the first accepted reply from a configured server.'
                    : 'No servers configured; systemd-timesyncd is stopped.'
                }
              />
            )}
          </Card>

          <Datagrid
            rowKey={(r) => r.host}
            onRefresh={refresh}
            columns={[
              {
                key: 'host', label: 'Server', sortable: true,
                render: (r) => <span className="cell-mono">{r.host}</span>,
              },
              {
                key: 'active', label: 'In Use',
                render: (r) => r.active
                  ? <Badge status="success">Selected</Badge>
                  : <span className="dim">—</span>,
              },
              {
                key: 'actions', label: '', width: 80,
                render: (r) => (
                  <span style={{ display: 'inline-flex', gap: 2 }}>
                    <Button variant="link-neutral" sm icon="pencil"
                      aria-label={`Edit ${r.host}`}
                      onClick={() => setModal({ kind: 'server', editing: r.host })} />
                    <Button variant="link-neutral" sm icon="trash"
                      aria-label={`Remove ${r.host}`}
                      onClick={() => removeServer(r.host)} />
                  </span>
                ),
              },
            ]}
            rows={rows}
            placeholder="No NTP servers configured; the clock is free-running."
          />
        </>
      )}
      <ServerModal open={!!modal} servers={servers}
        editing={modal && modal.editing} onClose={() => setModal(null)} onSaved={onSaved} />
    </Shell>
  );
}
