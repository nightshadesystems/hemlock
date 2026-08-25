'use client';
import { useCallback, useEffect, useRef, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, SearchSelect } from '@/components/ds/forms';
import { OperLabel } from '@/components/status';

/// Create ("New SVI") and edit share one dialog. An SVI *is* its
/// address — there is no separate object to name — so the VLAN is
/// fixed once the interface exists.
function SviModal({ open, svi, vlans, taken, onClose, onSaved }) {
  const editing = !!svi;
  const [vlan, setVlan] = useState('');
  const [address, setAddress] = useState('');
  const [mtu, setMtu] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setVlan(editing ? String(svi.vlan) : '');
    setAddress((editing && svi.address) || '');
    setMtu(editing && svi.mtu ? String(svi.mtu) : '');
    setError(null);
    setBusy(false);
  }, [open, editing, svi]);

  const submit = async () => {
    const id = parseInt(vlan, 10);
    if (!Number.isInteger(id) || id < 1 || id > 4094) {
      setError('Pick the VLAN this interface routes for.');
      return;
    }
    const set = { vlan: id };

    const addressNow = (editing && svi.address) || '';
    if (address !== addressNow) set.address = address.trim();
    const mtuNow = editing && svi.mtu ? String(svi.mtu) : '';
    if (mtu !== mtuNow) {
      if (mtu === '') {
        set.mtu = 0;
      } else {
        const bytes = parseInt(mtu, 10);
        if (!Number.isInteger(bytes) || bytes < 68 || bytes > 9216) {
          setError('MTU must be 68..9216 bytes (empty restores the default).');
          return;
        }
        set.mtu = bytes;
      }
    }
    if (!editing && !set.address) {
      setError('A new SVI needs an address (ip/prefix).');
      return;
    }

    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/svis/edit', {
        method: 'POST',
        body: JSON.stringify({ set: [set] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  if (!open) return null;

  // One SVI per VLAN, so a new one may only pick a VLAN without an
  // interface yet.
  const options = (vlans || [])
    .filter((v) => !taken.includes(v.id))
    .map((v) => ({ value: v.id, label: v.name ? `${v.id} — ${v.name}` : String(v.id) }));

  return (
    <Modal
      open={open}
      title={editing ? `Edit ${svi.name}` : 'New SVI'}
      size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy || !vlan}>
            Commit
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="VLAN" required
          helper={editing
            ? 'The interface is named after its VLAN and cannot move.'
            : options.length
              ? 'Only VLANs without a routed interface are listed.'
              : 'Every VLAN already has an SVI — create a VLAN first.'}>
          {editing ? (
            <Input className="mono" disabled readOnly
              value={`${svi.vlan}${svi.vlan_name ? ` — ${svi.vlan_name}` : ''}`} />
          ) : (
            <SearchSelect options={options} value={vlan}
              onChange={(v) => setVlan(String(v))} placeholder="Select VLAN…" />
          )}
        </FormField>
        <FormField label="Address" required={!editing} htmlFor="svi-address"
          helper="ip/prefix; empty removes the interface">
          <Input id="svi-address" className="mono" value={address} placeholder="10.0.10.1/24"
            autoFocus={editing} onChange={(e) => setAddress(e.target.value)} />
        </FormField>
        <FormField label="MTU" htmlFor="svi-mtu" helper="68..9216 bytes; empty restores the default">
          <Input id="svi-mtu" className="mono" value={mtu} placeholder="default"
            onChange={(e) => setMtu(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
      </div>
    </Modal>
  );
}

function DeleteModal({ open, vlans, onClose, onSaved }) {
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
      const result = await api('/api/svis/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: vlans }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  if (!open) return null;
  const names = vlans.map((v) => `Vlan${v}`).join(', ');
  const one = vlans.length === 1;

  return (
    <Modal
      open={open}
      title={one ? `Delete ${names}` : `Delete ${vlans.length} SVIs`}
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
        <span className="mono">{names}</span> {one ? 'is' : 'are'} removed from the configuration.
        The VLAN{one ? '' : 's'} and member ports stay — only the routed interface goes, so hosts
        in {one ? 'it' : 'them'} lose their gateway.
      </p>
    </Modal>
  );
}

export default function SvisPage() {
  const [svis, setSvis] = useState(null);
  const [vlans, setVlans] = useState([]);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null); // {kind:'new'} | {kind:'edit', svi} | {kind:'delete', vlans}
  const clearSel = useRef(null);

  const refresh = useCallback(() => {
    api('/api/svis')
      .then((r) => {
        setSvis(r.svis);
        setVlans(r.vlans || []);
      })
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    if (clearSel.current) clearSel.current();
    refresh();
  };

  const taken = (svis || []).map((s) => s.vlan);

  return (
    <Shell>
      <div className="page-header">
        <h2>SVIs</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!svis && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {svis && (
        <Datagrid
          selectable
          rowKey={(r) => r.vlan}
          onRefresh={refresh}
          actionBar={({ selected, clear }) => {
            clearSel.current = clear;
            const ids = [...selected].sort((a, b) => a - b);
            return (
              <>
                <Button variant="primary" sm icon="plus" onClick={() => setModal({ kind: 'new' })}>
                  New SVI
                </Button>
                <Button variant="danger-outline" sm disabled={ids.length === 0}
                  onClick={() => setModal({ kind: 'delete', vlans: ids })}>
                  Delete Selected{ids.length > 0 ? ` (${ids.length})` : ''}
                </Button>
              </>
            );
          }}
          columns={[
            {
              key: 'name', label: 'Interface', sortable: true,
              compare: (a, b) => a.vlan - b.vlan,
              render: (r) => <span className="cell-mono">{r.name}</span>,
            },
            {
              key: 'vlan', label: 'VLAN', sortable: true,
              render: (r) => (
                <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8, whiteSpace: 'nowrap' }}>
                  <span className="cell-mono">{r.vlan}</span>
                  {r.vlan_name && <Label>{r.vlan_name}</Label>}
                </span>
              ),
            },
            {
              key: 'address', label: 'Address',
              render: (r) => (r.address
                ? <span className="cell-mono">{r.address}</span>
                : <span className="dim">—</span>),
            },
            { key: 'mtu', label: 'MTU', render: (r) => <span className="cell-mono">{r.mtu || '—'}</span> },
            { key: 'oper_up', label: 'Link', sortable: true, render: (r) => <OperLabel up={r.oper_up} /> },
            {
              key: 'actions', label: '', width: 80,
              render: (r) => (
                <span style={{ display: 'inline-flex', gap: 2 }}>
                  <Button variant="link-neutral" sm icon="pencil" aria-label={`Edit ${r.name}`}
                    onClick={() => setModal({ kind: 'edit', svi: r })} />
                  <Button variant="link-neutral" sm icon="trash" aria-label={`Delete ${r.name}`}
                    onClick={() => setModal({ kind: 'delete', vlans: [r.vlan] })} />
                </span>
              ),
            },
          ]}
          rows={svis}
          placeholder="No routed VLAN interfaces configured."
        />
      )}
      <SviModal
        open={modal?.kind === 'new' || modal?.kind === 'edit'}
        svi={modal?.svi}
        vlans={vlans}
        taken={taken}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
      <DeleteModal
        open={modal?.kind === 'delete'}
        vlans={modal?.vlans || []}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
    </Shell>
  );
}
