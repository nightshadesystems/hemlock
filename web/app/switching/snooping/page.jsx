'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, shortName } from '@/lib/api';
import { Alert, Badge, Card, CardBlock, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Checkbox } from '@/components/ds/forms';

function GlobalModal({ open, family, state, onClose, onSaved }) {
  const [disabled, setDisabled] = useState(false);
  const [robustness, setRobustness] = useState('2');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open || !state) return;
    setDisabled(!state.enabled);
    setRobustness(String(state.robustness));
    setError(null);
    setBusy(false);
  }, [open, state]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/snooping/edit', {
        method: 'POST',
        body: JSON.stringify({
          family,
          disabled,
          robustness: parseInt(robustness, 10) || 2,
        }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title={`${family.toUpperCase()} snooping settings`} size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <Checkbox label={`Disable ${family.toUpperCase()} snooping globally`}
          checked={disabled} onChange={(e) => setDisabled(e.target.checked)} />
        <FormField label="Robustness" htmlFor="snoop-robustness" helper="1..3 (default 2)">
          <Input id="snoop-robustness" className="mono" value={robustness}
            onChange={(e) => setRobustness(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
      </div>
    </Modal>
  );
}

/// Per-VLAN settings; also serves "add VLAN".
function VlanModal({ open, family, vlan, onClose, onSaved }) {
  const editing = !!vlan;
  const [id, setId] = useState('');
  const [disabled, setDisabled] = useState(false);
  const [fastLeave, setFastLeave] = useState(false);
  const [querier, setQuerier] = useState(false);
  const [querierAddress, setQuerierAddress] = useState('');
  const [mrouters, setMrouters] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setId(editing ? String(vlan.vlan) : '');
    setDisabled(editing ? !vlan.enabled : false);
    setFastLeave(editing ? vlan.fast_leave : false);
    setQuerier(editing ? vlan.querier_enabled : false);
    setQuerierAddress(editing ? vlan.querier_address || '' : '');
    setMrouters(editing ? (vlan.static_mrouters || []).join(',') : '');
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
      const result = await api('/api/snooping/edit', {
        method: 'POST',
        body: JSON.stringify({
          family,
          set: [{
            vlan: parsed,
            disabled,
            fast_leave: fastLeave,
            querier,
            querier_address: querierAddress,
            mrouters: mrouters
              .split(',')
              .map((s) => s.trim())
              .filter(Boolean),
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
    <Modal open={open}
      title={editing ? `${family.toUpperCase()} Snooping · VLAN ${vlan.vlan}` : 'Add VLAN Settings'}
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy || !id}>
            Commit
          </Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="VLAN" required htmlFor="snoop-vlan" helper="1..4094">
          <Input id="snoop-vlan" className="mono" value={id} disabled={editing}
            onChange={(e) => setId(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <Checkbox label="Disable snooping on this VLAN" checked={disabled}
          onChange={(e) => setDisabled(e.target.checked)} />
        <Checkbox label="Fast-Leave" checked={fastLeave}
          onChange={(e) => setFastLeave(e.target.checked)} />
        <Checkbox label="Local Querier" checked={querier}
          onChange={(e) => setQuerier(e.target.checked)} />
        {querier && (
          <FormField label="Querier Address" htmlFor="snoop-querier-address"
            helper={family === 'igmp' ? 'IPv4; empty derives' : 'IPv6; empty derives'}>
            <Input id="snoop-querier-address" className="mono" value={querierAddress}
              onChange={(e) => setQuerierAddress(e.target.value)} />
          </FormField>
        )}
        <FormField label="Static Mrouter Ports" htmlFor="snoop-mrouters"
          helper="Comma-separated full names, e.g. Port-Channel1,Ethernet5">
          <Input id="snoop-mrouters" className="mono" value={mrouters}
            onChange={(e) => setMrouters(e.target.value)} style={{ maxWidth: 'none' }} />
        </FormField>
      </div>
    </Modal>
  );
}

export default function SnoopingPage() {
  const [family, setFamily] = useState('igmp');
  const [state, setState] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api(`/api/snooping?family=${family}`)
      .then(setState)
      .catch((e) => setError(e.message));
  }, [family]);
  useEffect(refresh, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const removeVlan = async (vlan) => {
    try {
      const result = await api('/api/snooping/edit', {
        method: 'POST',
        body: JSON.stringify({ family, delete: [vlan] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  const groups = state
    ? state.vlans.flatMap((v) => v.groups.map((g) => ({ ...g, vlan: v.vlan })))
    : [];

  return (
    <Shell>
      <div className="page-header">
        <h2>IGMP/MLD Snooping</h2>
        <span style={{ display: 'inline-flex', gap: 4 }}>
          <Button variant={family === 'igmp' ? 'primary' : 'outline'} sm
            onClick={() => { setFamily('igmp'); setState(null); }}>
            IGMP
          </Button>
          <Button variant={family === 'mld' ? 'primary' : 'outline'} sm
            onClick={() => { setFamily('mld'); setState(null); }}>
            MLD
          </Button>
        </span>
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
              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', width: '100%', gap: 16 }}>
                <span style={{ display: 'inline-flex', alignItems: 'center', gap: 12 }}>
                  Global
                  <Badge status={state.enabled ? 'success' : 'danger'}>
                    {state.enabled ? 'Enabled' : 'Disabled'}
                  </Badge>
                </span>
                <Button variant="outline" sm icon="pencil"
                  onClick={() => setModal({ kind: 'global' })}>
                  Settings
                </Button>
              </div>
            }
            style={{ marginBottom: 16 }}
          >
            <CardBlock title="Robustness Variable" text={String(state.robustness)} />
          </Card>

          <Datagrid
            rowKey={(r) => r.vlan}
            onRefresh={refresh}
            actionBar={() => (
              <Button variant="primary" sm icon="plus" onClick={() => setModal({ kind: 'vlan' })}>
                Add VLAN Settings
              </Button>
            )}
            columns={[
              { key: 'vlan', label: 'VLAN', sortable: true, render: (r) => <span className="cell-mono">{r.vlan}</span> },
              {
                key: 'enabled', label: 'Snooping',
                render: (r) => (
                  <Badge status={r.enabled ? 'success' : 'danger'}>
                    {r.enabled ? 'Enabled' : 'Disabled'}
                  </Badge>
                ),
              },
              {
                key: 'querier', label: 'Querier',
                render: (r) => r.querier_enabled
                  ? (
                    <span style={{ display: 'inline-flex', gap: 6, alignItems: 'center' }}>
                      <Badge status={r.querier_active ? 'success' : 'warning'}>
                        {r.querier_active ? 'Active' : 'Suppressed'}
                      </Badge>
                      {r.querier_address && <span className="cell-mono">{r.querier_address}</span>}
                    </span>
                  )
                  : <span className="dim">—</span>,
              },
              {
                key: 'fast_leave', label: 'Fast-Leave',
                render: (r) => r.fast_leave ? <Label>On</Label> : <span className="dim">—</span>,
              },
              {
                key: 'mrouters', label: 'Mrouter Ports',
                render: (r) => {
                  const all = [
                    ...(r.static_mrouters || []).map((p) => `${shortName(p)} (static)`),
                    ...(r.dynamic_mrouters || []).map((p) => `${shortName(p)} (dynamic)`),
                  ];
                  return all.length
                    ? <span className="cell-mono">{all.join(', ')}</span>
                    : <span className="dim">None</span>;
                },
              },
              {
                key: 'actions', label: '', width: 80,
                render: (r) => (
                  <span style={{ display: 'inline-flex', gap: 2 }}>
                    <Button variant="link-neutral" sm icon="pencil"
                      aria-label={`Edit VLAN ${r.vlan}`}
                      onClick={() => setModal({ kind: 'vlan', vlan: r })} />
                    <Button variant="link-neutral" sm icon="trash"
                      aria-label={`Clear VLAN ${r.vlan} settings`}
                      onClick={() => removeVlan(r.vlan)} />
                  </span>
                ),
              },
            ]}
            rows={state.vlans}
            placeholder="No per-VLAN snooping settings; snooping runs with defaults."
          />

          <h3 style={{ margin: '24px 0 12px' }}>Groups</h3>
          <Datagrid
            rowKey={(r) => `${r.vlan}-${r.group}`}
            onRefresh={refresh}
            columns={[
              { key: 'vlan', label: 'VLAN', sortable: true, render: (r) => <span className="cell-mono">{r.vlan}</span> },
              { key: 'group', label: 'Group', render: (r) => <span className="cell-mono">{r.group}</span> },
              { key: 'version', label: 'Version', render: (r) => <span className="cell-mono">v{r.version}</span> },
              {
                key: 'ports', label: 'Ports',
                render: (r) => <span className="cell-mono">{r.ports.map(shortName).join(', ')}</span>,
              },
            ]}
            rows={groups}
            placeholder="No multicast groups learned."
          />
        </>
      )}
      <GlobalModal open={!!modal && modal.kind === 'global'} family={family} state={state}
        onClose={() => setModal(null)} onSaved={onSaved} />
      <VlanModal open={!!modal && modal.kind === 'vlan'} family={family}
        vlan={modal && modal.kind === 'vlan' ? modal.vlan : null}
        onClose={() => setModal(null)} onSaved={onSaved} />
    </Shell>
  );
}
