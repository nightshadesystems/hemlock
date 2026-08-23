'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, shortName } from '@/lib/api';
import { Alert, Badge, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Select } from '@/components/ds/forms';

function StaticModal({ open, onClose, onSaved }) {
  const [mac, setMac] = useState('');
  const [vlan, setVlan] = useState('');
  const [target, setTarget] = useState('interface');
  const [port, setPort] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setMac('');
    setVlan('');
    setTarget('interface');
    setPort('');
    setError(null);
    setBusy(false);
  }, [open]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/mac-table/edit', {
        method: 'POST',
        body: JSON.stringify({
          set: [{
            mac,
            vlan: parseInt(vlan, 10),
            drop: target === 'drop',
            ...(target === 'interface' ? { port } : {}),
          }],
        }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title="Add static MAC" size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy}
            disabled={busy || !mac || !vlan || (target === 'interface' && !port)}>
            Commit
          </Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="MAC address" required htmlFor="mac-addr" helper="unicast, any common form">
          <Input id="mac-addr" className="mono" value={mac} placeholder="00:50:56:be:ef:01"
            autoFocus onChange={(e) => setMac(e.target.value)} />
        </FormField>
        <FormField label="VLAN" required htmlFor="mac-vlan" helper="1..4094">
          <Input id="mac-vlan" className="mono" value={vlan}
            onChange={(e) => setVlan(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <FormField label="Action" htmlFor="mac-target">
          <Select id="mac-target" value={target} onChange={(e) => setTarget(e.target.value)}
            options={[
              { value: 'interface', label: 'Forward to interface' },
              { value: 'drop', label: 'Drop' },
            ]} />
        </FormField>
        {target === 'interface' && (
          <FormField label="Interface" required htmlFor="mac-port" helper="e.g. Ethernet3 or Port-Channel1">
            <Input id="mac-port" className="mono" value={port}
              onChange={(e) => setPort(e.target.value)} />
          </FormField>
        )}
      </div>
    </Modal>
  );
}

function AgingModal({ open, current, onClose, onSaved }) {
  const [aging, setAging] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setAging(String(current));
    setError(null);
    setBusy(false);
  }, [open, current]);

  const submit = async (clear) => {
    setBusy(true);
    setError(null);
    try {
      const body = clear ? { clear_aging: true } : { aging_time: parseInt(aging, 10) };
      const result = await api('/api/mac-table/edit', {
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
    <Modal open={open} title="Aging time" size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="outline" onClick={() => submit(true)} disabled={busy}>
            Restore default (300)
          </Button>
          <Button variant="primary" onClick={() => submit(false)} loading={busy}
            disabled={busy || aging === ''}>
            Commit
          </Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <FormField label="Aging time" htmlFor="mac-aging" helper="Seconds; 0 disables aging (10..1000000 otherwise)">
        <Input id="mac-aging" className="mono" value={aging}
          onChange={(e) => setAging(e.target.value)} style={{ maxWidth: 160 }} />
      </FormField>
    </Modal>
  );
}

export default function MacTablePage() {
  const [table, setTable] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);
  // Server-side filters.
  const [vlan, setVlan] = useState('');
  const [port, setPort] = useState('');
  const [kind, setKind] = useState('');

  const refresh = useCallback(() => {
    const params = new URLSearchParams();
    if (vlan) params.set('vlan', vlan);
    if (port) params.set('port', port);
    if (kind) params.set('kind', kind);
    api(`/api/mac-table?${params}`)
      .then(setTable)
      .catch((e) => setError(e.message));
  }, [vlan, port, kind]);
  useEffect(refresh, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const flush = async () => {
    try {
      const body = {};
      if (vlan) body.vlan = parseInt(vlan, 10);
      if (port) body.port = port;
      const result = await api('/api/mac-table/flush', {
        method: 'POST',
        body: JSON.stringify(body),
      });
      setApplied([`${result.flushed} dynamic entr${result.flushed === 1 ? 'y' : 'ies'} flushed.`]);
      refresh();
    } catch (err) {
      setError(err.message);
    }
  };

  const removeStatic = async (entry) => {
    try {
      const result = await api('/api/mac-table/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: [{ mac: entry.mac, vlan: entry.vlan }] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  return (
    <Shell>
      <div className="page-header">
        <h2>MAC Table</h2>
        {table && (
          <span className="clr-secondary">
            aging {table.aging_time_secs === 0 ? 'disabled' : `${table.aging_time_secs}s`}
            {' · '}{table.total} entries
          </span>
        )}
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      <div style={{ display: 'flex', gap: 8, marginBottom: 12, alignItems: 'center' }}>
        <Input className="mono" value={vlan} placeholder="VLAN" aria-label="Filter VLAN"
          onChange={(e) => setVlan(e.target.value)} style={{ maxWidth: 100 }} />
        <Input className="mono" value={port} placeholder="Interface (full name)"
          aria-label="Filter interface"
          onChange={(e) => setPort(e.target.value)} style={{ maxWidth: 200 }} />
        <Select value={kind} onChange={(e) => setKind(e.target.value)} aria-label="Filter type"
          options={[
            { value: '', label: 'All types' },
            { value: 'dynamic', label: 'Dynamic' },
            { value: 'static', label: 'Static' },
          ]} />
        <span style={{ flex: 1 }} />
        <Button variant="primary" sm icon="plus" onClick={() => setModal({ kind: 'static' })}>
          Add static
        </Button>
        <Button variant="outline" sm onClick={() => setModal({ kind: 'aging' })}>
          Aging time
        </Button>
        <Button variant="danger-outline" sm onClick={flush}>
          Flush dynamic{vlan || port ? ' (filtered)' : ''}
        </Button>
      </div>
      {!table && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {table && (
        <Datagrid
          rowKey={(r) => `${r.vlan}-${r.mac}`}
          columns={[
            { key: 'vlan', label: 'VLAN', sortable: true, render: (r) => <span className="cell-mono">{r.vlan}</span> },
            { key: 'mac', label: 'MAC address', render: (r) => <span className="cell-mono">{r.mac}</span> },
            {
              key: 'type', label: 'Type',
              render: (r) => <Label>{r.is_static ? 'static' : 'dynamic'}</Label>,
            },
            {
              key: 'port', label: 'Port',
              render: (r) => r.drop
                ? <Badge status="danger">drop</Badge>
                : <span className="cell-mono">{shortName(r.port)}</span>,
            },
            {
              key: 'moves', label: 'Moves',
              render: (r) => r.is_static
                ? <span className="dim">—</span>
                : r.moves > 1
                  ? <Badge status="warning">{r.moves}</Badge>
                  : <span className="cell-mono">{r.moves}</span>,
            },
            {
              key: 'actions', label: '', width: 60,
              render: (r) => r.is_static && (
                <Button variant="link-neutral" sm icon="trash"
                  aria-label={`Delete static ${r.mac}`} onClick={() => removeStatic(r)} />
              ),
            },
          ]}
          rows={table.entries}
          placeholder="No MAC entries match."
        />
      )}
      <StaticModal open={!!modal && modal.kind === 'static'}
        onClose={() => setModal(null)} onSaved={onSaved} />
      <AgingModal open={!!modal && modal.kind === 'aging'}
        current={table ? table.aging_time_secs : 300}
        onClose={() => setModal(null)} onSaved={onSaved} />
    </Shell>
  );
}
