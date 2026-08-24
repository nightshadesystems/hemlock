'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, shortName, compareNames, formatUptime } from '@/lib/api';
import { Alert, Badge, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Select, SearchSelect } from '@/components/ds/forms';

/// Enable ("Enable Port") and edit share one dialog; the port is fixed
/// when editing. Empty fields revert to the defaults (maximum 1, protect).
function PortModal({ open, entry, interfaces, onClose, onSaved }) {
  const editing = !!entry;
  const [port, setPort] = useState('');
  const [maximum, setMaximum] = useState('');
  const [violation, setViolation] = useState('protect');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setPort(editing ? entry.port : '');
    setMaximum(editing ? String(entry.maximum) : '');
    setViolation(editing ? entry.violation : 'protect');
    setError(null);
    setBusy(false);
  }, [open, editing, entry]);

  const submit = async () => {
    const set = { interface: port, violation };
    if (maximum.trim()) {
      const parsed = parseInt(maximum, 10);
      if (!Number.isInteger(parsed) || parsed < 1 || parsed > 1024) {
        setError('Maximum must be 1..1024.');
        return;
      }
      set.maximum = parsed;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/port-security/edit', {
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
    <Modal open={open} title={editing ? `Port Security · ${entry.port}` : 'Enable Port Security'}
      size="sm" onClose={onClose}
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
        <FormField label="Interface" required>
          {editing ? (
            <Input className="mono" value={port} disabled style={{ maxWidth: 200 }} />
          ) : (
            <SearchSelect options={interfaces.map((name) => ({ value: name, label: name }))}
              value={port} onChange={setPort} placeholder="Select port…" />
          )}
        </FormField>
        <FormField label="Maximum MACs" htmlFor="ps-max" helper="1..1024; empty = 1">
          <Input id="ps-max" className="mono" value={maximum}
            onChange={(e) => setMaximum(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <FormField label="Violation Action" helper="protect drops offenders; shutdown errdisables">
          <Select options={['protect', 'shutdown']} value={violation}
            onChange={(e) => setViolation(e.target.value)} />
        </FormField>
      </div>
    </Modal>
  );
}

export default function PortSecurityPage() {
  const [ports, setPorts] = useState(null);
  const [interfaces, setInterfaces] = useState([]);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api('/api/port-security')
      .then((r) => setPorts(r.ports))
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

  const disable = async (port) => {
    setError(null);
    try {
      const result = await api('/api/port-security/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: [port] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  const clear = async (port) => {
    setError(null);
    try {
      const result = await api('/api/port-security/clear', {
        method: 'POST',
        body: JSON.stringify(port ? { port } : {}),
      });
      setApplied([`Cleared ${result.cleared} entr${result.cleared === 1 ? 'y' : 'ies'}.`]);
      refresh();
    } catch (err) {
      setError(err.message);
    }
  };

  return (
    <Shell>
      <div className="page-header">
        <h2>Port Security</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!ports && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {ports && (
        <Datagrid
          expandable
          rowKey={(r) => r.port}
          onRefresh={refresh}
          actionBar={() => (
            <>
              <Button variant="primary" sm icon="plus" onClick={() => setModal({ kind: 'new' })}>
                Enable Port
              </Button>
              <Button variant="outline" sm onClick={() => clear(null)}>Clear All Learned</Button>
            </>
          )}
          renderDetail={(r) => (
            <div style={{ padding: '4px 8px' }}>
              {r.learned.length === 0 && <span className="dim">No learned MACs.</span>}
              {r.learned.map((mac) => (
                <div key={mac.mac} style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
                  <span className="cell-mono">{mac.mac}</span>
                  <span className="dim">learned {formatUptime(mac.age_secs)} ago</span>
                </div>
              ))}
              {r.last_violation_mac && (
                <div style={{ marginTop: 8 }}>
                  <span className="dim">Last violation: </span>
                  <span className="cell-mono">{r.last_violation_mac}</span>
                  {r.last_violation_secs_ago != null && (
                    <span className="dim"> · {formatUptime(r.last_violation_secs_ago)} ago</span>
                  )}
                </div>
              )}
            </div>
          )}
          columns={[
            {
              key: 'port', label: 'Port', sortable: true,
              compare: (a, b) => compareNames(a.port, b.port),
              render: (r) => <span className="cell-mono">{shortName(r.port)}</span>,
            },
            { key: 'maximum', label: 'Max MACs', render: (r) => <span className="cell-mono">{r.maximum}</span> },
            {
              key: 'learned', label: 'Learned',
              render: (r) => <span className="cell-mono">{r.learned.length}</span>,
            },
            {
              key: 'violations', label: 'Violations',
              render: (r) => (
                <span className={r.violations > 0 ? 'cell-mono' : 'cell-mono dim'}>{r.violations}</span>
              ),
            },
            {
              key: 'violation', label: 'Action',
              render: (r) => <Label status={r.violation === 'shutdown' ? 'warning' : undefined}>{r.violation}</Label>,
            },
            {
              key: 'errdisabled', label: 'Status',
              render: (r) => r.errdisabled
                ? (
                  <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}>
                    <Badge status="danger">Errdisabled</Badge>
                    <Button variant="outline" sm onClick={() => clear(r.port)}>Re-enable</Button>
                  </span>
                )
                : <Badge status="success">OK</Badge>,
            },
            {
              key: 'actions', label: '', width: 80,
              render: (r) => (
                <span style={{ display: 'inline-flex', gap: 2 }}>
                  <Button variant="link-neutral" sm icon="pencil" aria-label={`Edit ${r.port}`}
                    onClick={() => setModal({ kind: 'edit', entry: r })} />
                  <Button variant="link-neutral" sm icon="trash" aria-label={`Disable ${r.port}`}
                    onClick={() => disable(r.port)} />
                </span>
              ),
            },
          ]}
          rows={ports}
          placeholder="Port security is not enabled on any port."
        />
      )}
      <PortModal
        open={!!modal && (modal.kind === 'new' || modal.kind === 'edit')}
        entry={modal && modal.kind === 'edit' ? modal.entry : null}
        interfaces={interfaces}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
    </Shell>
  );
}
