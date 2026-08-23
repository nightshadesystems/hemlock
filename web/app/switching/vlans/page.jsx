'use client';
import { useCallback, useEffect, useRef, useState } from 'react';
import Shell from '@/components/Shell';
import { api, shortName } from '@/lib/api';
import { Alert, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input } from '@/components/ds/forms';

function PortList({ ports }) {
  if (!ports || ports.length === 0) return <span className="dim">—</span>;
  return <span className="cell-mono">{ports.map(shortName).join(', ')}</span>;
}

/// Create ("New VLAN") and edit share one dialog; the id is fixed when
/// editing an existing VLAN.
function VlanModal({ open, vlan, onClose, onSaved }) {
  const editing = !!vlan;
  const [id, setId] = useState('');
  const [name, setName] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setId(editing ? String(vlan.id) : '');
    setName(editing ? vlan.name || '' : '');
    setError(null);
    setBusy(false);
  }, [open, editing, vlan]);

  const submit = async () => {
    const parsed = parseInt(id, 10);
    if (!Number.isInteger(parsed) || parsed < 1 || parsed > 4094) {
      setError('VLAN id must be 1..4094.');
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/vlans/edit', {
        method: 'POST',
        body: JSON.stringify({ set: [{ id: parsed, description: name }] }),
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
      title={editing ? `Edit VLAN ${vlan.id}` : 'New VLAN'}
      size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy || !id}>
            Commit
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="VLAN id" required htmlFor="vlan-id" helper="1..4094">
          <Input id="vlan-id" className="mono" value={id} disabled={editing} autoFocus={!editing}
            onChange={(e) => setId(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <FormField label="Name" htmlFor="vlan-name" helper="Optional; empty clears">
          <Input id="vlan-name" value={name} autoFocus={editing}
            onChange={(e) => setName(e.target.value)} style={{ maxWidth: 'none' }} />
        </FormField>
      </div>
    </Modal>
  );
}

function DeleteModal({ open, ids, onClose, onSaved }) {
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (open) {
      setError(null);
      setBusy(false);
    }
  }, [open]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/vlans/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: ids }),
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
      title={ids.length === 1 ? `Delete VLAN ${ids[0]}` : `Delete ${ids.length} VLANs`}
      size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="danger" onClick={submit} loading={busy} disabled={busy}>
            Delete
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <p>
        VLAN{ids.length === 1 ? '' : 's'} <span className="mono">{ids.join(', ')}</span> and any
        SVI addresses will be removed from the configuration. Member ports fall back to the
        default VLAN.
      </p>
    </Modal>
  );
}

export default function VlansPage() {
  const [vlans, setVlans] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null); // {kind:'new'} | {kind:'edit', vlan} | {kind:'delete', ids}
  const clearSel = useRef(null);

  const refresh = useCallback(() => {
    api('/api/vlans')
      .then((r) => setVlans(r.vlans))
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    if (clearSel.current) clearSel.current();
    refresh();
  };

  return (
    <Shell>
      <div className="page-header">
        <h2>VLANs</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!vlans && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {vlans && (
        <Datagrid
          selectable
          rowKey={(r) => r.id}
          actionBar={({ selected, clear }) => {
            clearSel.current = clear;
            const deletable = [...selected].filter((id) => id !== 1);
            return (
              <>
                <Button variant="primary" sm icon="plus" onClick={() => setModal({ kind: 'new' })}>
                  New VLAN
                </Button>
                <Button variant="danger-outline" sm disabled={deletable.length === 0}
                  onClick={() => setModal({ kind: 'delete', ids: deletable.sort((a, b) => a - b) })}>
                  Delete selected{deletable.length > 0 ? ` (${deletable.length})` : ''}
                </Button>
              </>
            );
          }}
          columns={[
            { key: 'id', label: 'VLAN', sortable: true, render: (r) => <span className="cell-mono">{r.id}</span> },
            {
              key: 'name', label: 'Name',
              render: (r) => (
                <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8, whiteSpace: 'nowrap' }}>
                  {r.name || <span className="dim">—</span>}
                  {r.id === 1 && <Label>default</Label>}
                </span>
              ),
            },
            {
              key: 'svi',
              label: 'SVI',
              render: (r) =>
                r.svi ? (
                  <span className="cell-mono">
                    {r.svi.name}
                    {r.svi.address ? ` · ${r.svi.address}` : ''}
                  </span>
                ) : (
                  <span className="dim">—</span>
                ),
            },
            { key: 'untagged', label: 'Untagged ports', render: (r) => <PortList ports={r.untagged} /> },
            { key: 'tagged', label: 'Tagged ports', render: (r) => <PortList ports={r.tagged} /> },
            {
              key: 'actions', label: '', width: 80,
              render: (r) => (
                <span style={{ display: 'inline-flex', gap: 2 }}>
                  <Button variant="link-neutral" sm icon="pencil" aria-label={`Edit VLAN ${r.id}`}
                    onClick={() => setModal({ kind: 'edit', vlan: r })} />
                  <Button variant="link-neutral" sm icon="trash" aria-label={`Delete VLAN ${r.id}`}
                    disabled={r.id === 1}
                    onClick={() => setModal({ kind: 'delete', ids: [r.id] })} />
                </span>
              ),
            },
          ]}
          rows={vlans}
          placeholder="No VLANs configured."
        />
      )}
      <VlanModal
        open={!!modal && (modal.kind === 'new' || modal.kind === 'edit')}
        vlan={modal && modal.kind === 'edit' ? modal.vlan : null}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
      <DeleteModal
        open={!!modal && modal.kind === 'delete'}
        ids={modal && modal.kind === 'delete' ? modal.ids : []}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
    </Shell>
  );
}
