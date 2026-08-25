'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, shortName } from '@/lib/api';
import { Alert, Badge, Card, CardBlock, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { Checkbox, FormField, Input } from '@/components/ds/forms';

// A neighbor's remaining hold time: the TTL it advertised, less how
// long ago we last heard from it. Counts down live between refreshes.
const remaining = (neighbor, tick) => {
  void tick;
  return Math.max(0, (neighbor.ttl || 0) - (neighbor.age_secs || 0));
};

// "mac" -> "MAC address" — the same phrasing `show lldp neighbors
// detail` prints.
const SUBTYPES = {
  mac: 'MAC address',
  'interface-name': 'interface name',
  'interface-alias': 'interface alias',
  'network-address': 'network address',
  'chassis-component': 'chassis component',
  'port-component': 'port component',
  'agent-circuit-id': 'agent circuit id',
  local: 'locally assigned',
};
const subtypeText = (s) => SUBTYPES[s] || (s || '').replace(/-/g, ' ');

/// Global settings: the off switch and the two timers.
function SettingsModal({ open, state, onClose, onSaved }) {
  const [disabled, setDisabled] = useState(false);
  const [txInterval, setTxInterval] = useState('30');
  const [holdMultiplier, setHoldMultiplier] = useState('4');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open || !state) return;
    setDisabled(!state.enabled);
    setTxInterval(String(state.tx_interval));
    setHoldMultiplier(String(state.hold_multiplier));
    setError(null);
    setBusy(false);
  }, [open, state]);

  const submit = async () => {
    const interval = parseInt(txInterval, 10);
    const multiplier = parseInt(holdMultiplier, 10);
    if (!Number.isInteger(interval) || interval < 5 || interval > 300) {
      setError('Tx interval must be 5..300 seconds.');
      return;
    }
    if (!Number.isInteger(multiplier) || multiplier < 2 || multiplier > 10) {
      setError('Hold multiplier must be 2..10.');
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/lldp/edit', {
        method: 'POST',
        body: JSON.stringify({
          disabled,
          tx_interval: interval,
          hold_multiplier: multiplier,
        }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  const ttl = (parseInt(txInterval, 10) || 0) * (parseInt(holdMultiplier, 10) || 0);

  return (
    <Modal open={open} title="LLDP settings" size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <Checkbox label="Disable LLDP globally" checked={disabled}
          onChange={(e) => setDisabled(e.target.checked)} />
        <FormField label="Tx Interval" htmlFor="lldp-tx-interval" helper="5..300 seconds (default 30)">
          <Input id="lldp-tx-interval" className="mono" value={txInterval}
            onChange={(e) => setTxInterval(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <FormField label="Hold Multiplier" htmlFor="lldp-hold" helper="2..10 (default 4)">
          <Input id="lldp-hold" className="mono" value={holdMultiplier}
            onChange={(e) => setHoldMultiplier(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <CardBlock title="Advertised hold time" text={ttl ? `${ttl} s` : '—'} />
      </div>
    </Modal>
  );
}

/// The neighbor detail block, matching `show lldp neighbors detail`.
function NeighborDetail({ neighbor }) {
  const rows = [
    ['Chassis ID', `${neighbor.chassis_id} (${subtypeText(neighbor.chassis_id_subtype)})`],
    ['Port ID', `${neighbor.port_id} (${subtypeText(neighbor.port_id_subtype)})`],
    ['Port Description', neighbor.port_description],
    ['System Name', neighbor.system_name],
    ['System Description', neighbor.system_description],
    ['Management Address', neighbor.management_address],
    ['TTL', `${neighbor.ttl} seconds`],
    ['Last heard', `${neighbor.age_secs} seconds ago`],
  ].filter(([, value]) => value);
  return (
    <div className="kv" style={{ padding: '4px 8px' }}>
      {rows.map(([label, value]) => (
        <div key={label} style={{ display: 'contents' }}>
          <div className="k">{label}</div>
          <div className="v mono">{value}</div>
        </div>
      ))}
    </div>
  );
}

export default function LldpPage() {
  const [state, setState] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [settings, setSettings] = useState(false);
  const [tick, setTick] = useState(0);

  const refresh = useCallback(() => {
    api('/api/lldp')
      .then(setState)
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  // The TTL column counts down between refreshes.
  useEffect(() => {
    const id = setInterval(() => setTick((t) => t + 1), 1000);
    return () => clearInterval(id);
  }, []);

  const onSaved = (result) => {
    setSettings(false);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  // The per-port grid drives one whole-set edit, so a single toggle
  // sends the full disabled-port list.
  const togglePort = async (port, enable) => {
    const disabled = new Set(
      (state.ports || []).filter((p) => !p.enabled).map((p) => p.port),
    );
    if (enable) disabled.delete(port);
    else disabled.add(port);
    try {
      const result = await api('/api/lldp/edit', {
        method: 'POST',
        body: JSON.stringify({ disabled_ports: [...disabled] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  const neighbors = state
    ? state.ports.flatMap((p) => p.neighbors.map((n) => ({ ...n, port: p.port })))
    : [];

  return (
    <Shell>
      <div className="page-header">
        <h2>LLDP</h2>
        <Button variant="outline" sm icon="pencil" onClick={() => setSettings(true)}>
          Settings
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
                Local Chassis
                <Badge status={state.enabled ? 'success' : 'danger'}>
                  {state.enabled ? 'Enabled' : 'Disabled'}
                </Badge>
              </span>
            }
            style={{ marginBottom: 16 }}
          >
            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 12 }}>
              <CardBlock title="Chassis ID" text={state.chassis_id || '—'} />
              <CardBlock title="System Name" text={state.system_name || '—'} />
              <CardBlock title="Management Address" text={state.management_address || '—'} />
              <CardBlock title="Tx Interval" text={`${state.tx_interval} s`} />
              <CardBlock title="Hold Multiplier" text={String(state.hold_multiplier)} />
              <CardBlock title="TTL" text={`${state.ttl} s`} />
            </div>
          </Card>

          <h3 style={{ margin: '0 0 12px' }}>Neighbors</h3>
          <Datagrid
            rowKey={(r) => `${r.port}-${r.chassis_id}-${r.port_id}`}
            onRefresh={refresh}
            expandable
            renderDetail={(r) => <NeighborDetail neighbor={r} />}
            columns={[
              {
                key: 'port', label: 'Port', sortable: true,
                render: (r) => <span className="cell-mono">{shortName(r.port)}</span>,
              },
              {
                key: 'system_name', label: 'Neighbor Device', sortable: true,
                render: (r) => (
                  <span className="cell-mono">{r.system_name || r.chassis_id}</span>
                ),
              },
              {
                key: 'port_id', label: 'Neighbor Port',
                render: (r) => <span className="cell-mono">{r.port_id}</span>,
              },
              {
                key: 'management_address', label: 'Management',
                render: (r) => r.management_address
                  ? <span className="cell-mono">{r.management_address}</span>
                  : <span className="dim">—</span>,
              },
              {
                key: 'ttl', label: 'TTL', width: 110,
                render: (r) => <span className="cell-mono">{remaining(r, tick)} s</span>,
              },
            ]}
            rows={neighbors}
            placeholder="No LLDP neighbors heard."
          />

          <h3 style={{ margin: '24px 0 12px' }}>Ports</h3>
          <Datagrid
            rowKey={(r) => r.port}
            onRefresh={refresh}
            compact
            columns={[
              {
                key: 'port', label: 'Port', sortable: true,
                render: (r) => <span className="cell-mono">{shortName(r.port)}</span>,
              },
              {
                key: 'enabled', label: 'State',
                render: (r) => (
                  <Badge status={r.enabled ? 'success' : 'danger'}>
                    {r.enabled ? 'Enabled' : 'Disabled'}
                  </Badge>
                ),
              },
              { key: 'frames_tx', label: 'Tx', render: (r) => <span className="cell-mono">{r.frames_tx}</span> },
              { key: 'frames_rx', label: 'Rx', render: (r) => <span className="cell-mono">{r.frames_rx}</span> },
              {
                key: 'frames_discarded', label: 'Discarded',
                render: (r) => <span className="cell-mono">{r.frames_discarded}</span>,
              },
              { key: 'ageouts', label: 'Ageouts', render: (r) => <span className="cell-mono">{r.ageouts}</span> },
              {
                key: 'neighbors', label: 'Neighbors',
                render: (r) => r.neighbors.length
                  ? <Label>{r.neighbors.length}</Label>
                  : <span className="dim">—</span>,
              },
              {
                key: 'actions', label: '', width: 110,
                render: (r) => (
                  <Button variant="link-neutral" sm
                    onClick={() => togglePort(r.port, !r.enabled)}>
                    {r.enabled ? 'Disable' : 'Enable'}
                  </Button>
                ),
              },
            ]}
            rows={state.ports}
            placeholder="No front-panel ports reported."
          />
        </>
      )}
      <SettingsModal open={settings} state={state}
        onClose={() => setSettings(false)} onSaved={onSaved} />
    </Shell>
  );
}
