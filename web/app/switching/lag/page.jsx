'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, shortName, compareNames, parseVlanList } from '@/lib/api';
import { Alert, Badge } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Select, Checkbox } from '@/components/ds/forms';

const MEMBER_STATUS = {
  bundled: 'success',
  individual: 'warning',
  standby: 'warning',
  down: 'danger',
};

function protocolLabel(lag) {
  if (!lag.lacp) return 'Static (mode on)';
  return `LACP ${lag.active_mode ? 'active' : 'passive'}`;
}

/// Per-member LACP runtime, shown from the summary row.
function DetailModal({ open, lag, onClose }) {
  if (!lag) return null;
  return (
    <Modal open={open} title={`Port-Channel${lag.group}`} onClose={onClose}
      footer={<Button variant="link-neutral" onClick={onClose}>Close</Button>}>
      <div className="clr-form-compact" style={{ marginBottom: 12 }}>
        <p>
          {protocolLabel(lag)} · {lag.bundled} of {lag.total} members bundled
          {lag.min_links > 0 ? ` · min-links ${lag.min_links}` : ''}
          {lag.fallback_mode
            ? ` · fallback ${lag.fallback_mode} (${lag.fallback_timeout_secs}s)` : ''}
          {lag.fallback_active ? ' · fallback active' : ''}
        </p>
      </div>
      <table className="table">
        <thead>
          <tr><th>Member</th><th>Status</th><th>Partner</th><th>Partner port</th></tr>
        </thead>
        <tbody>
          {lag.members.map((m) => (
            <tr key={m.port}>
              <td className="cell-mono">{shortName(m.port)}</td>
              <td><Badge status={MEMBER_STATUS[m.status] || undefined}>{m.status}</Badge></td>
              <td className="cell-mono">{m.partner_system || '—'}</td>
              <td className="cell-mono">{m.partner_system ? m.partner_port : '—'}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </Modal>
  );
}

/// Create ("New port-channel") and edit share one dialog.
function LagModal({ open, lag, interfaces, lags, onClose, onSaved }) {
  const editing = !!lag;
  const [group, setGroup] = useState('');
  const [description, setDescription] = useState('');
  const [protocol, setProtocol] = useState('active');
  const [rateFast, setRateFast] = useState(false);
  const [members, setMembers] = useState([]);
  const [minLinks, setMinLinks] = useState('');
  const [fallback, setFallback] = useState('');
  const [fallbackTimeout, setFallbackTimeout] = useState('90');
  const [mode, setMode] = useState('');
  const [trunkVlans, setTrunkVlans] = useState('');
  const [accessVlan, setAccessVlan] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setGroup(editing ? String(lag.group) : '');
    setDescription(editing ? lag.description || '' : '');
    setProtocol(editing ? (lag.lacp ? (lag.active_mode ? 'active' : 'passive') : 'on') : 'active');
    setRateFast(false);
    setMembers(editing ? lag.members.map((m) => m.port) : []);
    setMinLinks(editing && lag.min_links ? String(lag.min_links) : '');
    setFallback(editing ? lag.fallback_mode || '' : '');
    setFallbackTimeout(editing ? String(lag.fallback_timeout_secs || 90) : '90');
    setMode('');
    setTrunkVlans('');
    setAccessVlan('');
    setError(null);
    setBusy(false);
  }, [open, editing, lag]);

  // Client-side mirror of the commit-time rules.
  const memberTaken = (name) => {
    const owner = (lags || []).find(
      (l) => (!editing || l.group !== lag.group) && l.members.some((m) => m.port === name),
    );
    return owner ? `Po${owner.group}` : null;
  };
  const candidates = (interfaces || [])
    .filter((i) => i.kind === 'ethernet')
    .sort((a, b) => compareNames(a.name, b.name));

  const toggleMember = (name) =>
    setMembers((have) =>
      have.includes(name) ? have.filter((m) => m !== name) : [...have, name],
    );

  const submit = async () => {
    const parsed = parseInt(group, 10);
    if (!Number.isInteger(parsed) || parsed < 1 || parsed > 64) {
      setError('Channel-group number must be 1..64.');
      return;
    }
    if (members.length > 8) {
      setError('A channel group takes at most 8 members.');
      return;
    }
    for (const name of members) {
      const routed = candidates.find((i) => i.name === name && i.addresses.length > 0);
      if (routed) {
        setError(`${name} is routed — delete its address before adding it to a LAG.`);
        return;
      }
      const owner = memberTaken(name);
      if (owner) {
        setError(`${name} is already a member of ${owner}.`);
        return;
      }
    }
    setBusy(true);
    setError(null);
    try {
      const set = {
        group: parsed,
        description,
        members: members.map((port) => ({ port, mode: protocol, rate_fast: rateFast })),
      };
      if (minLinks !== '') set.min_links = parseInt(minLinks, 10);
      set.fallback = fallback;
      if (fallback) set.fallback_timeout = parseInt(fallbackTimeout, 10) || 90;
      if (mode) {
        set.mode = mode;
        if (mode === 'trunk' && trunkVlans !== '') set.trunk_vlans = parseVlanList(trunkVlans);
        if (mode === 'access' && accessVlan !== '') set.access_vlan = parseInt(accessVlan, 10);
      }
      const result = await api('/api/lags/edit', {
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
    <Modal open={open} title={editing ? `Edit Port-Channel${lag.group}` : 'New port-channel'}
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy || !group}>
            Commit
          </Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Group" required htmlFor="lag-group" helper="1..64">
          <Input id="lag-group" className="mono" value={group} disabled={editing}
            onChange={(e) => setGroup(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <FormField label="Description" htmlFor="lag-description">
          <Input id="lag-description" value={description}
            onChange={(e) => setDescription(e.target.value)} style={{ maxWidth: 'none' }} />
        </FormField>
        <FormField label="Protocol" htmlFor="lag-protocol">
          <Select id="lag-protocol" value={protocol} onChange={(e) => setProtocol(e.target.value)}
            options={[
              { value: 'active', label: 'LACP active' },
              { value: 'passive', label: 'LACP passive' },
              { value: 'on', label: 'Static (mode on)' },
            ]} />
        </FormField>
        {protocol !== 'on' && (
          <Checkbox label="Fast LACP rate (1s)" checked={rateFast}
            onChange={(e) => setRateFast(e.target.checked)} />
        )}
        <FormField label="Members" helper="Up to 8 Ethernet ports; all run the same mode.">
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(4, minmax(0,1fr))', gap: 4 }}>
            {candidates.map((i) => {
              const owner = memberTaken(i.name);
              return (
                <Checkbox key={i.name}
                  label={shortName(i.name) + (owner ? ` (${owner})` : '')}
                  checked={members.includes(i.name)}
                  disabled={!!owner || i.addresses.length > 0}
                  onChange={() => toggleMember(i.name)} />
              );
            })}
          </div>
        </FormField>
        <FormField label="Min links" htmlFor="lag-min-links" helper="0..8; empty keeps current">
          <Input id="lag-min-links" className="mono" value={minLinks}
            onChange={(e) => setMinLinks(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <FormField label="Fallback" htmlFor="lag-fallback">
          <Select id="lag-fallback" value={fallback} onChange={(e) => setFallback(e.target.value)}
            options={[
              { value: '', label: 'Off' },
              { value: 'static', label: 'Static' },
              { value: 'individual', label: 'Individual' },
            ]} />
        </FormField>
        {fallback && (
          <FormField label="Fallback timeout" htmlFor="lag-fallback-timeout" helper="1..900 seconds">
            <Input id="lag-fallback-timeout" className="mono" value={fallbackTimeout}
              onChange={(e) => setFallbackTimeout(e.target.value)} style={{ maxWidth: 120 }} />
          </FormField>
        )}
        <FormField label="Switchport" htmlFor="lag-mode" helper="Optional; empty keeps current">
          <Select id="lag-mode" value={mode} onChange={(e) => setMode(e.target.value)}
            options={[
              { value: '', label: 'No change' },
              { value: 'access', label: 'Access' },
              { value: 'trunk', label: 'Trunk' },
            ]} />
        </FormField>
        {mode === 'access' && (
          <FormField label="Access VLAN" htmlFor="lag-access-vlan">
            <Input id="lag-access-vlan" className="mono" value={accessVlan}
              onChange={(e) => setAccessVlan(e.target.value)} style={{ maxWidth: 120 }} />
          </FormField>
        )}
        {mode === 'trunk' && (
          <FormField label="Trunk VLANs" htmlFor="lag-trunk-vlans" helper="e.g. 10,20,30-32">
            <Input id="lag-trunk-vlans" className="mono" value={trunkVlans}
              onChange={(e) => setTrunkVlans(e.target.value)} />
          </FormField>
        )}
      </div>
    </Modal>
  );
}

function DeleteModal({ open, group, onClose, onSaved }) {
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (open) { setError(null); setBusy(false); }
  }, [open]);
  const submit = async () => {
    setBusy(true);
    try {
      const result = await api('/api/lags/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: [group] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };
  return (
    <Modal open={open} title={`Delete Port-Channel${group}`} size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="danger" onClick={submit} loading={busy} disabled={busy}>Delete</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <p>
        The port-channel and its members&apos; channel-group config will be removed; member ports
        return to standalone switching.
      </p>
    </Modal>
  );
}

export default function LagPage() {
  const [lags, setLags] = useState(null);
  const [interfaces, setInterfaces] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api('/api/lags').then((r) => setLags(r.lags)).catch((e) => setError(e.message));
    api('/api/interfaces').then((r) => setInterfaces(r.interfaces)).catch(() => {});
  }, []);
  useEffect(refresh, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  return (
    <Shell>
      <div className="page-header">
        <h2>Link Aggregation</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!lags && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {lags && (
        <Datagrid
          rowKey={(r) => r.group}
          actionBar={() => (
            <Button variant="primary" sm icon="plus" onClick={() => setModal({ kind: 'new' })}>
              New port-channel
            </Button>
          )}
          columns={[
            {
              key: 'group', label: 'Port-Channel', sortable: true,
              render: (r) => <span className="cell-mono">Po{r.group}</span>,
            },
            {
              key: 'state', label: 'State',
              render: (r) => (
                <Badge status={r.up ? 'success' : 'danger'}>
                  {r.up ? 'up' : 'down'}{r.fallback_active ? ' (fallback)' : ''}
                </Badge>
              ),
            },
            { key: 'protocol', label: 'Protocol', render: (r) => protocolLabel(r) },
            {
              key: 'bundled', label: 'Bundled',
              render: (r) => <span className="cell-mono">{r.bundled} / {r.total}</span>,
            },
            {
              key: 'min_links', label: 'Min links',
              render: (r) => <span className="cell-mono">{r.min_links || '—'}</span>,
            },
            {
              key: 'members', label: 'Members',
              render: (r) => (
                <span className="cell-mono">
                  {r.members.length
                    ? r.members.map((m) => shortName(m.port)).join(', ')
                    : <span className="dim">—</span>}
                </span>
              ),
            },
            { key: 'description', label: 'Description', render: (r) => r.description || <span className="dim">—</span> },
            {
              key: 'actions', label: '', width: 110,
              render: (r) => (
                <span style={{ display: 'inline-flex', gap: 2 }}>
                  <Button variant="link-neutral" sm icon="eye" aria-label={`Detail Po${r.group}`}
                    onClick={() => setModal({ kind: 'detail', lag: r })} />
                  <Button variant="link-neutral" sm icon="pencil" aria-label={`Edit Po${r.group}`}
                    onClick={() => setModal({ kind: 'edit', lag: r })} />
                  <Button variant="link-neutral" sm icon="trash" aria-label={`Delete Po${r.group}`}
                    onClick={() => setModal({ kind: 'delete', group: r.group })} />
                </span>
              ),
            },
          ]}
          rows={lags}
          placeholder="No port-channels configured."
        />
      )}
      <DetailModal open={!!modal && modal.kind === 'detail'}
        lag={modal && modal.kind === 'detail' ? modal.lag : null}
        onClose={() => setModal(null)} />
      <LagModal open={!!modal && (modal.kind === 'new' || modal.kind === 'edit')}
        lag={modal && modal.kind === 'edit' ? modal.lag : null}
        interfaces={interfaces} lags={lags}
        onClose={() => setModal(null)} onSaved={onSaved} />
      <DeleteModal open={!!modal && modal.kind === 'delete'}
        group={modal && modal.kind === 'delete' ? modal.group : 0}
        onClose={() => setModal(null)} onSaved={onSaved} />
    </Shell>
  );
}
