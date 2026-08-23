'use client';
import { useCallback, useEffect, useRef, useState } from 'react';
import Shell from '@/components/Shell';
import { api, formatSpeed, compareNames } from '@/lib/api';
import { Alert } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Select, MultiSelect } from '@/components/ds/forms';
import { OperLabel, AdminLabel, ModeLabel } from '@/components/status';

const NO_CHANGE = '';

function deriveMode(iface) {
  if (iface.kind === 'management') return 'management';
  if (iface.addresses && iface.addresses.length > 0) return 'routed';
  if (iface.switchport_mode === 'trunk') return 'trunk';
  if (iface.switchport_mode === 'dot1q-tunnel') return 'dot1q-tunnel';
  return 'access';
}

function vlanSummary(r) {
  if (r.kind !== 'ethernet' || (r.addresses && r.addresses.length > 0)) return '—';
  if (r.switchport_mode === 'trunk') {
    const vlans = (r.trunk_vlans || []).join(', ') || '—';
    return r.native_vlan ? `${vlans} · native ${r.native_vlan}` : vlans;
  }
  return String(r.access_vlan || 1);
}

/// Single- and bulk-edit dialog. In bulk mode every field defaults to
/// "no change" and only chosen fields are sent.
function EditModal({ open, targets, vlans, onClose, onSaved }) {
  const single = targets.length === 1;
  const target = single ? targets[0] : null;
  const management = single && target.kind === 'management';
  const hasManagement = targets.some((t) => t.kind === 'management');

  const [admin, setAdmin] = useState(NO_CHANGE);
  const [mode, setMode] = useState(NO_CHANGE);
  const [description, setDescription] = useState('');
  const [accessVlan, setAccessVlan] = useState('');
  const [trunkVlans, setTrunkVlans] = useState([]);
  const [nativeVlan, setNativeVlan] = useState('');
  const [address, setAddress] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setError(null);
    setBusy(false);
    if (single) {
      setDescription(target.description || '');
      setAdmin(target.admin_up ? 'up' : 'down');
      setMode(deriveMode(target));
      setAccessVlan(String(target.access_vlan || 1));
      setTrunkVlans(target.trunk_vlans || []);
      setNativeVlan(target.native_vlan ? String(target.native_vlan) : '');
      setAddress((target.addresses && target.addresses[0]) || '');
    } else {
      setDescription('');
      setAdmin(NO_CHANGE);
      setMode(NO_CHANGE);
      setAccessVlan('');
      setTrunkVlans([]);
      setNativeVlan('');
      setAddress('');
    }
  }, [open, single, target]);

  if (!open) return null;

  const parseVlanField = (text, label) => {
    const id = parseInt(text, 10);
    if (!Number.isInteger(id) || id < 1 || id > 4094) {
      throw new Error(`${label} must be a VLAN id (1..4094)`);
    }
    return id;
  };

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const body = { names: targets.map((t) => t.name) };
      if (single) {
        if (description !== (target.description || '')) body.description = description;
      }
      if (admin !== NO_CHANGE) body.admin_up = admin === 'up';
      if (mode === 'access' || mode === 'trunk' || mode === 'dot1q-tunnel' || mode === 'routed') {
        if (hasManagement && mode !== 'routed') {
          throw new Error('Management ports are not switchports — deselect them to change mode.');
        }
        body.mode = mode;
        if ((mode === 'access' || mode === 'dot1q-tunnel') && accessVlan !== '') {
          body.access_vlan = parseVlanField(accessVlan, mode === 'access' ? 'Access VLAN' : 'S-VLAN');
        }
        if (mode === 'trunk') {
          if (single || trunkVlans.length > 0) body.trunk_vlans = trunkVlans;
          if (nativeVlan !== '') body.native_vlan = parseVlanField(nativeVlan, 'Native VLAN');
        }
        if (mode === 'routed') {
          if (!address) throw new Error('A routed interface needs an address (ip/prefix).');
          body.address = address;
        }
      } else if (management && address !== ((target.addresses && target.addresses[0]) || '')) {
        // Management interfaces are always routed; only the address moves.
        body.address = address;
      }
      const result = await api('/api/interfaces/edit', {
        method: 'POST',
        body: JSON.stringify(body),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  const title = single ? `Edit ${target.name}` : `Edit ${targets.length} Interfaces`;
  const modeOptions = [
    ...(single ? [] : [{ value: NO_CHANGE, label: 'No Change' }]),
    { value: 'access', label: 'Access' },
    { value: 'trunk', label: 'Trunk' },
    { value: 'dot1q-tunnel', label: 'Dot1q Tunnel (QinQ)' },
    { value: 'routed', label: 'Routed' },
  ];
  const vlanOptions = (vlans || []).map((v) => ({
    value: v.id,
    label: v.name ? `${v.id} — ${v.name}` : String(v.id),
  }));

  return (
    <Modal
      open={open}
      title={title}
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>
            Commit Changes
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      {!single && (
        <p className="clr-secondary" style={{ marginBottom: 12 }}>
          Applies to: <span className="mono">{targets.map((t) => t.name).join(', ')}</span>
        </p>
      )}
      <div className="clr-form-compact">
        {single && (
          <FormField label="Description" htmlFor="if-description">
            <Input id="if-description" value={description}
              onChange={(e) => setDescription(e.target.value)} style={{ maxWidth: 'none' }} />
          </FormField>
        )}
        <FormField label="Admin State" htmlFor="if-admin">
          <Select id="if-admin" value={admin} onChange={(e) => setAdmin(e.target.value)}
            options={[
              ...(single ? [] : [{ value: NO_CHANGE, label: 'No Change' }]),
              { value: 'up', label: 'Enabled' },
              { value: 'down', label: 'Shutdown' },
            ]} />
        </FormField>
        {management ? (
          <FormField label="Address" htmlFor="if-address" helper="ip/prefix; empty clears">
            <Input id="if-address" className="mono" value={address} placeholder="10.42.10.9/24"
              onChange={(e) => setAddress(e.target.value)} />
          </FormField>
        ) : (
          <>
            <FormField label="Mode" htmlFor="if-mode">
              <Select id="if-mode" value={mode} onChange={(e) => setMode(e.target.value)}
                options={modeOptions} />
            </FormField>
            {mode === 'access' && (
              <FormField label="Access VLAN" htmlFor="if-access-vlan">
                <Input id="if-access-vlan" className="mono" value={accessVlan}
                  placeholder={single ? undefined : 'unchanged'}
                  onChange={(e) => setAccessVlan(e.target.value)} style={{ maxWidth: 120 }} />
              </FormField>
            )}
            {mode === 'dot1q-tunnel' && (
              <FormField label="S-VLAN" htmlFor="if-svlan"
                helper="Customer frames tunnel inside this VLAN. Raise the MTU if frames exceed 1504 bytes.">
                <Input id="if-svlan" className="mono" value={accessVlan}
                  placeholder={single ? undefined : 'unchanged'}
                  onChange={(e) => setAccessVlan(e.target.value)} style={{ maxWidth: 120 }} />
              </FormField>
            )}
            {mode === 'trunk' && (
              <>
                <FormField label="Trunk VLANs" htmlFor="if-trunk-vlans"
                  helper={vlanOptions.length ? undefined : 'No VLANs configured yet — create them on the VLANs page.'}>
                  <MultiSelect options={vlanOptions} values={trunkVlans}
                    onChange={setTrunkVlans}
                    placeholder={single ? 'Select VLANs…' : 'Unchanged'} />
                </FormField>
                <FormField label="Native VLAN" htmlFor="if-native-vlan"
                  helper="Untagged VLAN; empty leaves it unset">
                  <Input id="if-native-vlan" className="mono" value={nativeVlan}
                    placeholder="unset"
                    onChange={(e) => setNativeVlan(e.target.value)} style={{ maxWidth: 120 }} />
                </FormField>
              </>
            )}
            {mode === 'routed' && (
              <FormField label="Address" required htmlFor="if-address2" helper="ip/prefix">
                <Input id="if-address2" className="mono" value={address} placeholder="10.42.10.9/24"
                  onChange={(e) => setAddress(e.target.value)} />
              </FormField>
            )}
          </>
        )}
      </div>
    </Modal>
  );
}

export default function InterfacesPage() {
  const [interfaces, setInterfaces] = useState(null);
  const [vlans, setVlans] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [editing, setEditing] = useState(null); // array of iface rows
  const clearSel = useRef(null);

  const refresh = useCallback(() => {
    api('/api/interfaces')
      .then((r) => setInterfaces(r.interfaces.filter((i) => i.kind !== 'vlan')))
      .catch((e) => setError(e.message));
    api('/api/vlans').then((r) => setVlans(r.vlans)).catch(() => {});
  }, []);
  useEffect(refresh, [refresh]);

  const onSaved = (result) => {
    setEditing(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    if (clearSel.current) clearSel.current();
    refresh();
  };

  return (
    <Shell>
      <div className="page-header">
        <h2>Interfaces</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!interfaces && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {interfaces && (
        <Datagrid
          selectable
          rowKey={(r) => r.name}
          onRefresh={refresh}
          actionBar={({ selected, clear }) => {
            clearSel.current = clear;
            return (
              <Button sm disabled={selected.size === 0}
                onClick={() => setEditing(interfaces.filter((i) => selected.has(i.name)))}>
                Edit Selected{selected.size > 0 ? ` (${selected.size})` : ''}
              </Button>
            );
          }}
          columns={[
            {
              key: 'name', label: 'Interface', sortable: true,
              compare: (a, b) => compareNames(a.name, b.name),
              render: (r) => <span className="cell-mono">{r.name}</span>,
            },
            { key: 'description', label: 'Description', render: (r) => r.description || <span className="dim">—</span> },
            { key: 'media', label: 'Type', render: (r) => (r.media ? <span className="cell-mono">{r.media}</span> : <span className="dim">—</span>) },
            { key: 'mode', label: 'Mode', render: (r) => <ModeLabel iface={r} /> },
            { key: 'vlans', label: 'VLANs', render: (r) => <span className="cell-mono">{vlanSummary(r)}</span> },
            { key: 'addresses', label: 'Address', render: (r) => (r.addresses && r.addresses.length > 0 ? <span className="cell-mono">{r.addresses.join(', ')}</span> : <span className="dim">—</span>) },
            { key: 'admin_up', label: 'Admin', render: (r) => <AdminLabel up={r.admin_up} /> },
            { key: 'oper_up', label: 'Link', sortable: true, render: (r) => <OperLabel up={r.oper_up} /> },
            { key: 'speed_mbps', label: 'Speed', sortable: true, render: (r) => <span className="cell-mono">{formatSpeed(r.speed_mbps)}</span> },
            { key: 'mtu', label: 'MTU', render: (r) => <span className="cell-mono">{r.mtu || '—'}</span> },
            {
              key: 'actions', label: '', width: 44,
              render: (r) => (
                <Button variant="link-neutral" sm icon="pencil" aria-label={`Edit ${r.name}`}
                  onClick={() => setEditing([r])} />
              ),
            },
          ]}
          rows={interfaces}
          pageSize={24}
          placeholder="No interfaces reported."
        />
      )}
      <EditModal
        open={!!editing}
        targets={editing || []}
        vlans={vlans}
        onClose={() => setEditing(null)}
        onSaved={onSaved}
      />
    </Shell>
  );
}
