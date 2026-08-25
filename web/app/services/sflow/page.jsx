'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, shortName, compareNames } from '@/lib/api';
import { Alert, Badge, Card, CardBlock } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Select, Checkbox } from '@/components/ds/forms';

const MAX_COLLECTORS = 2;
const DEFAULT_PORT = 6343;

// The sampler divides by a power of two, so the picker offers exactly
// the rates the ASIC can take.
const RATES = [];
for (let rate = 256; rate <= 1048576; rate *= 2) RATES.push(rate);

// "1 in 16384" at 1 Gb/s of 64-byte frames is roughly this many
// samples a second — the helper that makes a rate mean something.
const samplesPerSecond = (rate, speedMbps = 1000) => {
  const packetsPerSecond = (speedMbps * 1_000_000) / (64 + 20) / 8;
  return packetsPerSecond / rate;
};

/// Sample rate and polling interval.
function SettingsModal({ open, state, onClose, onSaved }) {
  const [rate, setRate] = useState('16384');
  const [polling, setPolling] = useState('30');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open || !state) return;
    setRate(String(state.sample_rate || 16384));
    setPolling(String(state.polling_interval || 30));
    setError(null);
    setBusy(false);
  }, [open, state]);

  const submit = async () => {
    const interval = parseInt(polling, 10);
    if (!Number.isInteger(interval) || interval < 5 || interval > 300) {
      setError('Polling interval must be 5..300 seconds.');
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/sflow/edit', {
        method: 'POST',
        body: JSON.stringify({
          sample_rate: parseInt(rate, 10),
          polling_interval: interval,
        }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  const estimate = samplesPerSecond(parseInt(rate, 10) || 1);

  return (
    <Modal open={open} title="sFlow settings" size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Sample Rate" htmlFor="sflow-rate"
          helper={`1 in N; roughly ${estimate.toFixed(1)} samples/s per saturated 1G port`}>
          <Select id="sflow-rate" value={rate} onChange={(e) => setRate(e.target.value)}
            options={RATES.map((n) => ({ value: String(n), label: `1 in ${n}` }))} />
        </FormField>
        <FormField label="Polling Interval" htmlFor="sflow-polling"
          helper="Seconds between counter samples; 5..300 (default 30)">
          <Input id="sflow-polling" className="mono" value={polling}
            onChange={(e) => setPolling(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
      </div>
    </Modal>
  );
}

/// Add/edit one collector. The whole list is sent on commit.
function CollectorModal({ open, collectors, editing, onClose, onSaved }) {
  const [address, setAddress] = useState('');
  const [port, setPort] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setAddress(editing ? editing.address : '');
    setPort(editing && editing.port && editing.port !== DEFAULT_PORT ? String(editing.port) : '');
    setError(null);
    setBusy(false);
  }, [open, editing]);

  const submit = async () => {
    const value = address.trim();
    if (!value) {
      setError('Enter a collector address.');
      return;
    }
    let parsedPort = 0;
    if (port.trim()) {
      parsedPort = parseInt(port, 10);
      if (!Number.isInteger(parsedPort) || parsedPort < 1 || parsedPort > 65535) {
        setError('Port must be 1..65535.');
        return;
      }
    }
    const entry = { address: value, port: parsedPort };
    const next = editing
      ? collectors.map((c) => (c.address === editing.address ? entry : { address: c.address, port: c.port }))
      : [...collectors.map((c) => ({ address: c.address, port: c.port })), entry];
    if (next.length > MAX_COLLECTORS) {
      setError(`At most ${MAX_COLLECTORS} collectors.`);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/sflow/edit', {
        method: 'POST',
        body: JSON.stringify({ collectors: next }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title={editing ? `Collector · ${editing.address}` : 'Add Collector'} size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy || !address}>
            Commit
          </Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Address" required htmlFor="sflow-collector" helper="IPv4 or IPv6">
          <Input id="sflow-collector" className="mono" value={address} disabled={!!editing}
            onChange={(e) => setAddress(e.target.value)} />
        </FormField>
        <FormField label="Port" htmlFor="sflow-port" helper={`Empty uses the default ${DEFAULT_PORT}`}>
          <Input id="sflow-port" className="mono" value={port}
            onChange={(e) => setPort(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
      </div>
    </Modal>
  );
}

/// The per-port sampling picker: every front-panel port, with the
/// disabled ones checked.
function PortsModal({ open, state, onClose, onSaved }) {
  const [disabled, setDisabled] = useState([]);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open || !state) return;
    setDisabled(state.disabled_ports || []);
    setError(null);
    setBusy(false);
  }, [open, state]);

  const all = state
    ? [...new Set([...(state.enabled_ports || []), ...(state.disabled_ports || [])])].sort(compareNames)
    : [];

  const toggle = (port) =>
    setDisabled((current) =>
      current.includes(port) ? current.filter((p) => p !== port) : [...current, port],
    );

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/sflow/edit', {
        method: 'POST',
        body: JSON.stringify({ disabled_ports: disabled }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title="Sampling ports" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <p className="dim" style={{ marginTop: 0 }}>
        Sampling runs on every front-panel port; check a port to exclude it.
      </p>
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(120px, 1fr))', gap: 4 }}>
        {all.map((port) => (
          <Checkbox key={port} label={shortName(port)} checked={disabled.includes(port)}
            onChange={() => toggle(port)} />
        ))}
      </div>
    </Modal>
  );
}

export default function SflowPage() {
  const [state, setState] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api('/api/sflow')
      .then(setState)
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  // Sample and datagram counters move while traffic flows.
  useEffect(() => {
    const id = setInterval(refresh, 5_000);
    return () => clearInterval(id);
  }, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const collectors = state ? state.collectors : [];

  const removeCollector = async (address) => {
    try {
      const result = await api('/api/sflow/edit', {
        method: 'POST',
        body: JSON.stringify({
          collectors: collectors
            .filter((c) => c.address !== address)
            .map((c) => ({ address: c.address, port: c.port })),
        }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  return (
    <Shell>
      <div className="page-header">
        <h2>sFlow</h2>
        {state && state.enabled && (
          <>
            <Button variant="outline" sm icon="pencil" onClick={() => setModal({ kind: 'settings' })}>
              Settings
            </Button>
            <Button variant="outline" sm icon="network-settings"
              onClick={() => setModal({ kind: 'ports' })}>
              Sampling Ports
            </Button>
          </>
        )}
        <Button variant="primary" sm icon="plus"
          disabled={collectors.length >= MAX_COLLECTORS}
          onClick={() => setModal({ kind: 'collector' })}>
          Add Collector
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
      {state && !state.supported && (
        <Alert status="danger" style={{ marginBottom: 16 }}>
          sFlow sampling is not supported by this platform&apos;s SAI.
        </Alert>
      )}
      {state && !state.enabled && state.supported && (
        <Alert status="info" style={{ marginBottom: 16 }}>
          sFlow is off until a collector is configured; sampling then runs on every
          front-panel port.
        </Alert>
      )}
      {state && state.enabled && (
        <Card
          header={
            <span style={{ display: 'inline-flex', alignItems: 'center', gap: 12 }}>
              Sampler
              <Badge status="success">Enabled</Badge>
            </span>
          }
          style={{ marginBottom: 16 }}
        >
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(4, 1fr)', gap: 12 }}>
            <CardBlock title="Agent Address"
              text={`${state.agent_address || '—'}${state.agent_interface ? ` (${state.agent_interface})` : ''}`} />
            <CardBlock title="Sample Rate" text={`1 in ${state.sample_rate}`} />
            <CardBlock title="Polling Interval" text={`${state.polling_interval} s`} />
            <CardBlock title="Sampling Ports"
              text={`${(state.enabled_ports || []).length} enabled, ${(state.disabled_ports || []).length} disabled`} />
            <CardBlock title="Samples Taken" text={String(state.samples_taken)} />
            <CardBlock title="Counter Samples" text={String(state.counter_samples)} />
            <CardBlock title="Datagrams Sent" text={String(state.datagrams_sent)} />
            <CardBlock title="Datagrams Failed" text={String(state.datagrams_failed)} />
          </div>
        </Card>
      )}
      {state && (
        <>
          <Datagrid
            rowKey={(r) => r.address}
            onRefresh={refresh}
            columns={[
              {
                key: 'address', label: 'Collector',
                render: (r) => <span className="cell-mono">{r.address}</span>,
              },
              {
                key: 'port', label: 'Port',
                render: (r) => <span className="cell-mono">{r.port || DEFAULT_PORT}</span>,
              },
              {
                key: 'actions', label: '', width: 80,
                render: (r) => (
                  <span style={{ display: 'inline-flex', gap: 2 }}>
                    <Button variant="link-neutral" sm icon="pencil"
                      aria-label={`Edit ${r.address}`}
                      onClick={() => setModal({ kind: 'collector', editing: r })} />
                    <Button variant="link-neutral" sm icon="trash"
                      aria-label={`Remove ${r.address}`}
                      onClick={() => removeCollector(r.address)} />
                  </span>
                ),
              },
            ]}
            rows={collectors}
            placeholder="No collectors configured; sFlow is off."
          />

          {state.enabled && (state.disabled_ports || []).length > 0 && (
            <>
              <h3 style={{ margin: '24px 0 12px' }}>Excluded Ports</h3>
              <Datagrid
                rowKey={(r) => r}
                compact
                columns={[
                  { key: 'port', label: 'Port', render: (r) => <span className="cell-mono">{shortName(r)}</span> },
                  { key: 'state', label: 'Sampling', render: () => <Badge status="danger">Disabled</Badge> },
                ]}
                rows={[...state.disabled_ports].sort(compareNames)}
                placeholder="Every port is sampled."
              />
            </>
          )}
        </>
      )}
      <SettingsModal open={!!modal && modal.kind === 'settings'} state={state}
        onClose={() => setModal(null)} onSaved={onSaved} />
      <CollectorModal open={!!modal && modal.kind === 'collector'} collectors={collectors}
        editing={modal && modal.kind === 'collector' ? modal.editing : null}
        onClose={() => setModal(null)} onSaved={onSaved} />
      <PortsModal open={!!modal && modal.kind === 'ports'} state={state}
        onClose={() => setModal(null)} onSaved={onSaved} />
    </Shell>
  );
}
