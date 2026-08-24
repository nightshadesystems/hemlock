'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Card, CardBlock, Label } from '@/components/ds/misc';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { Checkbox, FormField, Input, Textarea } from '@/components/ds/forms';

function GroupModal({ open, group, onClose, onSaved }) {
  const editing = !!group;
  const [iface, setIface] = useState('');
  const [vrid, setVrid] = useState('');
  const [addresses, setAddresses] = useState('');
  const [priority, setPriority] = useState('');
  const [interval, setInterval_] = useState('');
  const [preempt, setPreempt] = useState(true);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setIface(editing ? group.interface : '');
    setVrid(editing ? String(group.group) : '');
    setAddresses(editing ? (group.addresses || []).join('\n') : '');
    setPriority(editing ? group.priority || '' : '');
    setInterval_(editing ? group.advertisement_interval || '' : '');
    setPreempt(editing ? group.preempt !== false : true);
    setError(null);
    setBusy(false);
  }, [open, editing, group]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    const set = {
      interface: iface.trim(),
      group: parseInt(vrid, 10) || 0,
      addresses: addresses.split(/\s+/).map((s) => s.trim()).filter(Boolean),
      preempt,
    };
    if (priority.toString().trim()) set.priority = parseInt(priority, 10);
    if (interval.toString().trim()) set.advertisement_interval = parseInt(interval, 10);
    try {
      onSaved(await api('/api/vrrp/edit', {
        method: 'POST',
        body: JSON.stringify({ set: [set] }),
      }));
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open}
      title={editing ? `Edit ${group.interface} group ${group.group}` : 'New VRRP Group'}
      size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy}
            disabled={busy || !iface.trim() || !vrid.trim() || !addresses.trim()}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Interface" required htmlFor="vrrp-if"
          helper="An addressed SVI or L3 port, e.g. Vlan100">
          <Input id="vrrp-if" className="mono" value={iface} disabled={editing}
            autoFocus={!editing} onChange={(e) => setIface(e.target.value)}
            style={{ maxWidth: 160 }} />
        </FormField>
        <FormField label="Group" required htmlFor="vrrp-group" helper="1..255">
          <Input id="vrrp-group" className="mono" value={vrid} disabled={editing}
            onChange={(e) => setVrid(e.target.value)} style={{ maxWidth: 100 }} />
        </FormField>
        <FormField label="Virtual addresses" required htmlFor="vrrp-vips"
          helper="One IPv4 VIP per line, inside the interface's subnet">
          <Textarea id="vrrp-vips" className="mono" rows={2} value={addresses}
            onChange={(e) => setAddresses(e.target.value)} style={{ maxWidth: 240 }} />
        </FormField>
        <FormField label="Priority" htmlFor="vrrp-pri" helper="1..254; empty = 100">
          <Input id="vrrp-pri" className="mono" value={priority}
            onChange={(e) => setPriority(e.target.value)} style={{ maxWidth: 100 }} />
        </FormField>
        <FormField label="Advertisement interval" htmlFor="vrrp-adv" helper="Seconds, 1..40; empty = 1">
          <Input id="vrrp-adv" className="mono" value={interval}
            onChange={(e) => setInterval_(e.target.value)} style={{ maxWidth: 100 }} />
        </FormField>
        <FormField>
          <Checkbox label="Preempt" checked={preempt}
            onChange={(e) => setPreempt(e.target.checked)} />
        </FormField>
      </div>
    </Modal>
  );
}

function StateBadge({ state }) {
  if (!state) return <Label>Init</Label>;
  const status = state === 'Master' ? 'success' : state === 'Backup' ? 'info' : 'warning';
  return <Label status={status}>{state}</Label>;
}

export default function VrrpPage() {
  const [groups, setGroups] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api('/api/vrrp')
      .then((r) => {
        setGroups(r.groups);
        setError(null);
      })
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const remove = async (group) => {
    setError(null);
    try {
      onSaved(await api('/api/vrrp/edit', {
        method: 'POST',
        body: JSON.stringify({
          delete: [{ interface: group.interface, group: parseInt(group.group, 10) }],
        }),
      }));
    } catch (err) {
      setError(err.message);
    }
  };

  return (
    <Shell>
      <div className="page-header" style={{ display: 'flex', alignItems: 'center' }}>
        <h2 style={{ marginRight: 'auto' }}>VRRP</h2>
        <Button sm variant="primary" icon="plus" onClick={() => setModal({})}>New Group</Button>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!groups && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {groups && groups.length === 0 && (
        <Alert status="info">No VRRP groups configured.</Alert>
      )}
      {groups && groups.length > 0 && (
        <div className="card-grid">
          {groups.map((group) => (
            <Card
              key={`${group.interface}-${group.group}`}
              header={
                <div style={{ display: 'flex', alignItems: 'center', width: '100%', gap: 12 }}>
                  <span className="card-title" style={{ marginRight: 'auto' }}>
                    {group.interface} · group {group.group}
                  </span>
                  <StateBadge state={group.state} />
                  <Button variant="link-neutral" sm icon="pencil"
                    aria-label="Edit group" onClick={() => setModal({ group })} />
                  <Button variant="link-neutral" sm icon="trash"
                    aria-label="Delete group" onClick={() => remove(group)} />
                </div>
              }
            >
              <div style={{ display: 'grid', gridTemplateColumns: 'repeat(2, 1fr)', gap: 12 }}>
                <CardBlock title="Virtual Address(es)"
                  text={(group.addresses || []).join(', ') || '—'} />
                <CardBlock title="Priority"
                  text={`${group.priority || 100}${group.effective_priority ? ` (effective ${group.effective_priority})` : ''}`} />
                <CardBlock title="Preempt" text={group.preempt ? 'enabled' : 'disabled'} />
                <CardBlock title="Virtual MAC" text={group.virtual_mac || `00:00:5e:00:01:${Number(group.group).toString(16).padStart(2, '0')}`} />
              </div>
            </Card>
          ))}
        </div>
      )}
      <GroupModal open={!!modal} group={modal?.group} onClose={() => setModal(null)} onSaved={onSaved} />
    </Shell>
  );
}
