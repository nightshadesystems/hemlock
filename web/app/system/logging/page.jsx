'use client';
import { useCallback, useEffect, useRef, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Badge, Card, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Select } from '@/components/ds/forms';

const MAX_HOSTS = 4;

const LEVELS = [
  'emergencies',
  'alerts',
  'critical',
  'errors',
  'warnings',
  'notifications',
  'informational',
  'debugging',
];

// Syslog severities, in the order the numbers run.
const SEVERITY_NAMES = [
  'emergency',
  'alert',
  'critical',
  'error',
  'warning',
  'notice',
  'info',
  'debug',
];

const severityLabel = (n) => SEVERITY_NAMES[n] || 'unknown';
const severityStatus = (n) =>
  n <= 3 ? 'danger' : n === 4 ? 'warning' : n === 5 ? 'info' : undefined;

const validAddress = (text) =>
  /^[0-9]{1,3}(\.[0-9]{1,3}){3}$/.test(text) || /^[0-9A-Fa-f:]+$/.test(text);

const stamp = (unix) => {
  if (!unix) return '—';
  // The CLI prints UTC; the console matches it so the two read alike.
  return new Date(unix * 1000).toISOString().replace('T', ' ').replace(/\..*/, '');
};

const hostLabel = (host) =>
  `${host.address.includes(':') ? `[${host.address}]` : host.address}:${host.port} (${host.protocol})`;

/// Add and edit share one dialog; the whole collector list is sent on
/// commit, because the config block is a set.
function HostModal({ open, hosts, editing, level, onClose, onSaved }) {
  const [address, setAddress] = useState('');
  const [port, setPort] = useState('514');
  const [protocol, setProtocol] = useState('udp');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setAddress(editing ? editing.address : '');
    setPort(editing ? String(editing.port) : '514');
    setProtocol(editing ? editing.protocol : 'udp');
    setError(null);
    setBusy(false);
  }, [open, editing]);

  const portNumber = parseInt(port, 10);
  const problems = [];
  if (!validAddress(address.trim())) problems.push('Enter an IPv4 or IPv6 address.');
  if (!(portNumber >= 1 && portNumber <= 65535)) problems.push('Port must be 1..65535.');
  if (
    !editing &&
    hosts.some((h) => h.address === address.trim())
  )
    problems.push('That collector is already configured.');

  const submit = async () => {
    const host = { address: address.trim(), port: portNumber, protocol };
    const next = editing
      ? hosts.map((h) => (h.address === editing.address ? host : h))
      : [...hosts, host];
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/system/logging/edit', {
        method: 'POST',
        body: JSON.stringify({ hosts: next, level }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      title={editing ? `Collector · ${editing.address}` : 'Add Collector'}
      size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy}
            disabled={busy || problems.length > 0}>
            Commit
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      {problems.length > 0 && (
        <Alert status="warning" sm items={problems} style={{ marginBottom: 12 }} />
      )}
      <div className="clr-form-compact">
        <FormField label="Address" required htmlFor="log-address"
          helper={`IPv4 or IPv6; up to ${MAX_HOSTS} collectors`}>
          <Input id="log-address" className="mono" autoFocus value={address}
            disabled={!!editing}
            onChange={(e) => setAddress(e.target.value)} />
        </FormField>
        <FormField label="Port" htmlFor="log-port" helper="Syslog default is 514.">
          <Input id="log-port" className="mono" type="number" min={1} max={65535} value={port}
            onChange={(e) => setPort(e.target.value)} />
        </FormField>
        <FormField label="Protocol" htmlFor="log-protocol"
          helper="TCP queues on disk when the collector is unreachable; UDP does not.">
          <Select id="log-protocol" value={protocol} onChange={(e) => setProtocol(e.target.value)}
            options={[{ value: 'udp', label: 'UDP' }, { value: 'tcp', label: 'TCP' }]} />
        </FormField>
      </div>
    </Modal>
  );
}

export default function LoggingPage() {
  const [state, setState] = useState(null);
  const [session, setSession] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);
  const [paused, setPaused] = useState(false);
  const [filter, setFilter] = useState('');
  const [lines, setLines] = useState(200);
  const tailRef = useRef(null);

  const refresh = useCallback(() => {
    api(`/api/system/logging?count=${lines}`)
      .then(setState)
      .catch((e) => setError(e.message));
  }, [lines]);
  useEffect(refresh, [refresh]);
  useEffect(() => {
    api('/api/session').then(setSession).catch(() => {});
  }, []);

  // The tail moves on its own; poll while the page is open and not
  // paused, so a line an operator is reading does not scroll away.
  useEffect(() => {
    if (paused) return undefined;
    const id = setInterval(refresh, 5_000);
    return () => clearInterval(id);
  }, [refresh, paused]);

  const isAdmin = !session || session.admin;
  const hosts = (state && state.hosts) || [];
  const level = (state && state.level) || 'informational';

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const commit = async (body) => {
    try {
      onSaved(
        await api('/api/system/logging/edit', {
          method: 'POST',
          body: JSON.stringify(body),
        }),
      );
    } catch (err) {
      setError(err.message);
    }
  };

  const entries = ((state && state.entries) || []).filter((entry) => {
    if (!filter) return true;
    const needle = filter.toLowerCase();
    return (
      entry.message.toLowerCase().includes(needle) ||
      (entry.tag || '').toLowerCase().includes(needle)
    );
  });

  // Keep the newest line in view unless the operator paused to read.
  useEffect(() => {
    if (!paused && tailRef.current) {
      tailRef.current.scrollTop = tailRef.current.scrollHeight;
    }
  }, [entries.length, paused]);

  return (
    <Shell>
      <div className="page-header">
        <h2>Logging</h2>
        <Button variant="primary" sm icon="plus"
          disabled={!isAdmin || hosts.length >= MAX_HOSTS}
          title={isAdmin ? undefined : 'Operator role: this console is read-only.'}
          onClick={() => setModal({})}>
          Add Collector
        </Button>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)} items={applied}
          style={{ marginBottom: 16 }} />
      )}
      {!state && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {state && (
        <>
          <Card
            header={
              <span style={{ display: 'inline-flex', alignItems: 'center', gap: 12 }}>
                Remote Collectors
                <Badge status={hosts.length ? 'success' : undefined}>
                  {hosts.length ? `forwarding at ${level} and above` : 'local journal only'}
                </Badge>
              </span>
            }
            style={{ marginBottom: 16 }}
          >
            <div className="clr-form-compact" style={{ padding: '12px 16px 0' }}>
              <FormField label="Forwarding Level" htmlFor="log-level"
                helper="Everything at this severity and above is forwarded.">
                <Select id="log-level" value={level} disabled={!isAdmin}
                  onChange={(e) => commit({ hosts, level: e.target.value })}
                  options={LEVELS.map((l) => ({ value: l, label: l }))} />
              </FormField>
            </div>
            <Datagrid
              rowKey={(r) => r.address}
              columns={[
                {
                  key: 'address', label: 'Address', sortable: true,
                  render: (r) => <span className="cell-mono">{r.address}</span>,
                },
                {
                  key: 'port', label: 'Port',
                  render: (r) => <span className="cell-mono">{r.port}</span>,
                },
                {
                  key: 'protocol', label: 'Protocol',
                  render: (r) => <Label>{r.protocol.toUpperCase()}</Label>,
                },
                {
                  key: 'actions', label: '', width: 80,
                  render: (r) => (
                    <span style={{ display: 'inline-flex', gap: 2 }}>
                      <Button variant="link-neutral" sm icon="pencil" disabled={!isAdmin}
                        aria-label={`Edit ${r.address}`}
                        onClick={() => setModal({ editing: r })} />
                      <Button variant="link-neutral" sm icon="trash" disabled={!isAdmin}
                        aria-label={`Remove ${r.address}`}
                        onClick={() =>
                          commit({ hosts: hosts.filter((h) => h.address !== r.address), level })
                        } />
                    </span>
                  ),
                },
              ]}
              rows={hosts}
              placeholder="No collectors configured; logs stay in the local journal."
              footerText={hosts.map(hostLabel).join(', ')}
            />
          </Card>

          <Card
            header={
              <span style={{ display: 'flex', alignItems: 'center', gap: 12, flexWrap: 'wrap' }}>
                <span>Live Tail</span>
                <Input className="mono" placeholder="Filter…" value={filter}
                  style={{ maxWidth: 220 }}
                  aria-label="Filter log lines"
                  onChange={(e) => setFilter(e.target.value)} />
                <Select value={String(lines)} aria-label="Lines"
                  onChange={(e) => setLines(parseInt(e.target.value, 10))}
                  options={[
                    { value: '50', label: '50 lines' },
                    { value: '200', label: '200 lines' },
                    { value: '1000', label: '1000 lines' },
                  ]} />
                <Button variant={paused ? 'primary' : 'outline'} sm
                  icon={paused ? 'play' : 'pause'}
                  onClick={() => setPaused((p) => !p)}>
                  {paused ? 'Resume' : 'Pause'}
                </Button>
                <Button variant="link-neutral" sm icon="refresh" aria-label="Refresh"
                  onClick={refresh} />
              </span>
            }
          >
            {!state.journal_available && (
              <Alert status="warning" sm style={{ margin: 16 }}>
                The system journal is not readable here.
              </Alert>
            )}
            {state.journal_available && (
              <div ref={tailRef} className="mono"
                style={{
                  maxHeight: 420,
                  overflow: 'auto',
                  padding: 16,
                  fontSize: 12,
                  lineHeight: 1.6,
                }}>
                {entries.length === 0 && <span className="dim">No matching lines.</span>}
                {entries.map((entry, index) => (
                  <div key={index} style={{ display: 'flex', gap: 8, alignItems: 'baseline' }}>
                    <span className="dim" style={{ whiteSpace: 'nowrap' }}>{stamp(entry.time)}</span>
                    {entry.severity <= 5 && (
                      <Label status={severityStatus(entry.severity)}>
                        {severityLabel(entry.severity)}
                      </Label>
                    )}
                    <span style={{ whiteSpace: 'nowrap' }}>
                      {entry.tag}{entry.pid ? `[${entry.pid}]` : ''}:
                    </span>
                    <span style={{ wordBreak: 'break-word' }}>{entry.message}</span>
                  </div>
                ))}
              </div>
            )}
          </Card>
        </>
      )}
      <HostModal open={!!modal} hosts={hosts} editing={modal && modal.editing} level={level}
        onClose={() => setModal(null)} onSaved={onSaved} />
    </Shell>
  );
}
