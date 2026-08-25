'use client';
import { useCallback, useEffect, useRef, useState } from 'react';
import Shell from '@/components/Shell';
import { api, shortName } from '@/lib/api';
import { Alert, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { Checkbox, FormField, Input, Select } from '@/components/ds/forms';

const TRUST_MODES = ['untrusted', 'dscp', 'cos'];
/// How many samples the counter sparklines keep. At the 5s poll below
/// that is a little over two minutes of history.
const HISTORY = 30;

/// A shaper rate for display, matching the CLI's `100 Mbps` form.
function displayRate(bps) {
  if (!bps) return null;
  for (const [factor, unit] of [[1e9, 'Gbps'], [1e6, 'Mbps'], [1e3, 'Kbps']]) {
    if (bps >= factor && bps % factor === 0) return `${bps / factor} ${unit}`;
  }
  return `${bps} bps`;
}

/// Validate a shaper rate the way `parse_shape_rate` does, so a bad one
/// never reaches mgmtd. Returns the rate in bits/sec.
function parseRate(text) {
  const m = /^(\d+)([kmg]?)$/i.exec(text.trim());
  if (!m) throw new Error(`bad rate "${text}" (<bps>[k|m|g])`);
  const scale = { '': 1, k: 1e3, m: 1e6, g: 1e9 }[m[2].toLowerCase()];
  const bps = parseInt(m[1], 10) * scale;
  if (bps < 64000) throw new Error(`rate "${text}" is below the 64k shaper granularity floor`);
  return bps;
}

/// A rolling counter series as an inline sparkline. Deltas, not totals:
/// what an operator watches is the rate, and a monotonic counter would
/// draw a flat diagonal.
function Sparkline({ series, color, label }) {
  const deltas = [];
  for (let i = 1; i < series.length; i++) {
    deltas.push(Math.max(0, series[i] - series[i - 1]));
  }
  if (deltas.length < 2) {
    return <span className="cell-mono dim" title={label}>—</span>;
  }
  const peak = Math.max(...deltas, 1);
  const width = 72;
  const height = 18;
  const step = width / (deltas.length - 1);
  const points = deltas
    .map((v, i) => `${(i * step).toFixed(1)},${(height - (v / peak) * (height - 2) - 1).toFixed(1)}`)
    .join(' ');
  return (
    <svg width={width} height={height} role="img" aria-label={label}
      style={{ verticalAlign: 'middle' }}>
      <title>{`${label} — peak ${peak}/interval`}</title>
      <polyline points={points} fill="none" stroke={color} strokeWidth="1.5"
        strokeLinejoin="round" strokeLinecap="round" />
    </svg>
  );
}

/// The expanded per-queue table: effective program plus live counters.
function QueueTable({ port, history }) {
  const queues = [...port.queues].sort((a, b) => b.queue - a.queue);
  const series = (queue, field) => (history[`${port.port}/${queue}/${field}`] || []);
  return (
    <table className="datagrid-table" style={{ fontSize: 12 }}>
      <thead>
        <tr className="datagrid-row">
          {['Queue', 'Mode', 'Weight', 'Shaper', 'WRED', 'ECN', 'Tx Packets', 'Dropped',
            'WRED Drops', 'ECN Marked', 'Tx', 'Drops'].map((h) => (
              <th key={h} className="datagrid-column">{h}</th>
            ))}
        </tr>
      </thead>
      <tbody>
        {queues.map((q) => (
          <tr key={q.queue} className="datagrid-row">
            <td className="datagrid-cell cell-mono">{q.queue}</td>
            <td className="datagrid-cell">
              {q.mode === 'strict' ? <Label status="warning">strict</Label> : <Label>dwrr</Label>}
            </td>
            <td className="datagrid-cell cell-mono">{q.weight == null ? '—' : q.weight}</td>
            <td className="datagrid-cell cell-mono">{q.shaper || '—'}</td>
            <td className="datagrid-cell cell-mono">{q.wred_profile || '—'}</td>
            <td className="datagrid-cell">
              {q.wred_profile ? (q.ecn ? <Label status="info">yes</Label> : <Label>no</Label>) : '—'}
            </td>
            <td className="datagrid-cell cell-mono">{q.tx_packets}</td>
            <td className={`datagrid-cell cell-mono${q.dropped ? '' : ' dim'}`}>{q.dropped}</td>
            <td className={`datagrid-cell cell-mono${q.wred_dropped ? '' : ' dim'}`}>
              {q.wred_dropped}
            </td>
            <td className={`datagrid-cell cell-mono${q.ecn_marked ? '' : ' dim'}`}>
              {q.ecn_marked}
            </td>
            <td className="datagrid-cell">
              <Sparkline series={series(q.queue, 'tx_packets')}
                color="var(--clr-color-success-700, #2f8a4c)"
                label={`Queue ${q.queue} transmit rate`} />
            </td>
            <td className="datagrid-cell">
              <Sparkline series={series(q.queue, 'dropped')}
                color="var(--clr-color-danger-700, #c92100)"
                label={`Queue ${q.queue} drop rate`} />
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/// Blank per-queue form rows: absent fields leave the config alone, so
/// the modal starts from what the port actually has.
function queueForms(port, queueCount) {
  return Array.from({ length: queueCount }, (_, index) => {
    const q = port ? port.queues.find((entry) => entry.queue === index) : null;
    return {
      queue: index,
      mode: q && q.mode === 'strict' ? 'strict' : 'dwrr',
      weight: q && q.weight != null && q.weight !== 1 ? String(q.weight) : '',
      shape: q && q.shape_bps ? bpsToText(q.shape_bps) : '',
      wred_profile: (q && q.wred_profile) || '',
    };
  });
}

/// The suffixed config form of a rate ("100m"), which is what the edit
/// API takes.
function bpsToText(bps) {
  for (const [factor, suffix] of [[1e9, 'g'], [1e6, 'm'], [1e3, 'k']]) {
    if (bps >= factor && bps % factor === 0) return `${bps / factor}${suffix}`;
  }
  return String(bps);
}

/// Edit one port's whole QoS program. The client-side checks mirror
/// §1.3 exactly — strict/weight exclusivity, strict contiguity from the
/// top queue down, and queue shaper ≤ port shaper — so the operator
/// sees the problem before the commit does.
function PortModal({ open, port, meta, onClose, onSaved }) {
  const [trust, setTrust] = useState('untrusted');
  const [defaultTc, setDefaultTc] = useState('0');
  const [shape, setShape] = useState('');
  const [queues, setQueues] = useState([]);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setTrust(port ? port.trust : 'untrusted');
    setDefaultTc(port ? String(port.default_tc) : '0');
    setShape(port && port.shape_bps ? bpsToText(port.shape_bps) : '');
    setQueues(queueForms(port, meta.queue_count || 8));
    setError(null);
    setBusy(false);
  }, [open, port, meta.queue_count]);

  const setQueue = (index, patch) =>
    setQueues((rows) => rows.map((row) => (row.queue === index ? { ...row, ...patch } : row)));

  const validate = () => {
    let portRate = null;
    if (shape.trim()) portRate = parseRate(shape);
    const strict = [];
    for (const row of queues) {
      if (row.mode === 'strict' && row.weight.trim()) {
        throw new Error(`Queue ${row.queue}: strict and weight are mutually exclusive.`);
      }
      if (row.mode === 'strict') strict.push(row.queue);
      if (row.weight.trim()) {
        const weight = parseInt(row.weight, 10);
        if (!Number.isInteger(weight) || weight < 1 || weight > 127) {
          throw new Error(`Queue ${row.queue}: weight must be 1..127.`);
        }
      }
      if (row.shape.trim()) {
        const rate = parseRate(row.shape);
        if (portRate != null && rate > portRate) {
          throw new Error(
            `Queue ${row.queue}: shaper ${row.shape} exceeds the port shaper ${shape}.`,
          );
        }
        if (!meta.queue_shaper_supported) {
          throw new Error('Per-queue shapers are not supported by this platform’s SAI.');
        }
      }
      if (row.wred_profile && !meta.wred_supported) {
        throw new Error('WRED is not supported by this platform’s SAI.');
      }
    }
    // The Helix4 scheduler tree can only express strict priority on the
    // top queues, so the strict set must be a run down from the highest.
    if (strict.length > 0) {
      const top = (meta.queue_count || 8) - 1;
      const expected = [];
      for (let q = top - strict.length + 1; q <= top; q++) expected.push(q);
      if ([...strict].sort((a, b) => a - b).join(',') !== expected.join(',')) {
        throw new Error('Strict queues must be the highest-numbered queues.');
      }
    }
  };

  const submit = async () => {
    try {
      validate();
    } catch (err) {
      setError(err.message);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/qos/ports/edit', {
        method: 'POST',
        body: JSON.stringify({
          set: [{
            name: port.port,
            trust,
            default_tc: defaultTc,
            shape,
            queues: queues.map((row) => ({
              queue: row.queue,
              mode: row.mode,
              weight: row.mode === 'strict' ? '' : row.weight,
              shape: row.shape,
              wred_profile: row.wred_profile,
            })),
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
    <Modal open={open} size="lg" onClose={onClose} title={port ? `QoS on ${port.port}` : ''}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact" style={{ display: 'flex', gap: 16, flexWrap: 'wrap' }}>
        <FormField label="Trust mode" htmlFor="qos-trust">
          <Select id="qos-trust" options={TRUST_MODES} value={trust}
            onChange={(e) => setTrust(e.target.value)} style={{ maxWidth: 160 }} />
        </FormField>
        <FormField label="Default TC" htmlFor="qos-tc" helper="0..7">
          <Input id="qos-tc" className="mono" value={defaultTc} style={{ maxWidth: 90 }}
            onChange={(e) => setDefaultTc(e.target.value)} />
        </FormField>
        <FormField label="Port shaper" htmlFor="qos-shape" helper="e.g. 800m; empty = unshaped">
          <Input id="qos-shape" className="mono" value={shape} style={{ maxWidth: 140 }}
            onChange={(e) => setShape(e.target.value)} />
        </FormField>
      </div>
      <table className="datagrid-table" style={{ fontSize: 12, marginTop: 12 }}>
        <thead>
          <tr className="datagrid-row">
            {['Queue', 'Mode', 'Weight', 'Shaper', 'WRED profile'].map((h) => (
              <th key={h} className="datagrid-column">{h}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {[...queues].sort((a, b) => b.queue - a.queue).map((row) => (
            <tr key={row.queue} className="datagrid-row">
              <td className="datagrid-cell cell-mono">{row.queue}</td>
              <td className="datagrid-cell">
                <Select options={['dwrr', 'strict']} value={row.mode}
                  onChange={(e) => setQueue(row.queue, {
                    mode: e.target.value,
                    weight: e.target.value === 'strict' ? '' : row.weight,
                  })} style={{ maxWidth: 110 }} />
              </td>
              <td className="datagrid-cell">
                <Input className="mono" value={row.weight} placeholder="1"
                  disabled={row.mode === 'strict'} style={{ maxWidth: 80 }}
                  onChange={(e) => setQueue(row.queue, { weight: e.target.value })} />
              </td>
              <td className="datagrid-cell">
                <Input className="mono" value={row.shape} placeholder="—"
                  disabled={!meta.queue_shaper_supported} style={{ maxWidth: 110 }}
                  onChange={(e) => setQueue(row.queue, { shape: e.target.value })} />
              </td>
              <td className="datagrid-cell">
                <Select value={row.wred_profile} disabled={!meta.wred_supported}
                  options={[{ value: '', label: '—' },
                    ...(meta.wred_profiles || []).map((n) => ({ value: n, label: n }))]}
                  onChange={(e) => setQueue(row.queue, { wred_profile: e.target.value })}
                  style={{ maxWidth: 140 }} />
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </Modal>
  );
}

/// Apply one queue template to a selection of ports in a single commit.
function BulkModal({ open, ports, meta, onClose, onSaved }) {
  const [queue, setQueue] = useState('3');
  const [mode, setMode] = useState('dwrr');
  const [weight, setWeight] = useState('');
  const [shape, setShape] = useState('');
  const [profile, setProfile] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setError(null);
    setBusy(false);
  }, [open]);

  const submit = async () => {
    const index = parseInt(queue, 10);
    if (!Number.isInteger(index) || index < 0 || index >= (meta.queue_count || 8)) {
      setError(`Queue must be 0..${(meta.queue_count || 8) - 1}.`);
      return;
    }
    if (mode === 'strict' && weight.trim()) {
      setError('Strict and weight are mutually exclusive.');
      return;
    }
    try {
      if (shape.trim()) parseRate(shape);
    } catch (err) {
      setError(err.message);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/qos/ports/edit', {
        method: 'POST',
        body: JSON.stringify({
          set: ports.map((name) => ({
            name,
            queues: [{
              queue: index,
              mode,
              weight: mode === 'strict' ? '' : weight,
              shape,
              wred_profile: profile,
            }],
          })),
        }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} size="md" onClose={onClose}
      title={`Apply queue template to ${ports.length} port${ports.length === 1 ? '' : 's'}`}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <Alert status="info" sm style={{ marginBottom: 12 }}>
        {ports.map(shortName).join(', ')}
      </Alert>
      <div className="clr-form-compact">
        <FormField label="Queue" helper={`0..${(meta.queue_count || 8) - 1}`}>
          <Input className="mono" value={queue} style={{ maxWidth: 90 }}
            onChange={(e) => setQueue(e.target.value)} />
        </FormField>
        <FormField label="Mode">
          <Select options={['dwrr', 'strict']} value={mode} style={{ maxWidth: 140 }}
            onChange={(e) => setMode(e.target.value)} />
        </FormField>
        <FormField label="Weight" helper="1..127; empty leaves the default (1)">
          <Input className="mono" value={weight} disabled={mode === 'strict'}
            style={{ maxWidth: 90 }} onChange={(e) => setWeight(e.target.value)} />
        </FormField>
        <FormField label="Shaper" helper="e.g. 100m; empty clears it">
          <Input className="mono" value={shape} disabled={!meta.queue_shaper_supported}
            style={{ maxWidth: 140 }} onChange={(e) => setShape(e.target.value)} />
        </FormField>
        <FormField label="WRED profile">
          <Select value={profile} disabled={!meta.wred_supported}
            options={[{ value: '', label: '—' },
              ...(meta.wred_profiles || []).map((n) => ({ value: n, label: n }))]}
            onChange={(e) => setProfile(e.target.value)} style={{ maxWidth: 180 }} />
        </FormField>
      </div>
    </Modal>
  );
}

export default function QosInterfacesPage() {
  const [state, setState] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);
  const [bulk, setBulk] = useState(null);
  // Rolling counter history per queue, keyed "<port>/<queue>/<field>".
  const [history, setHistory] = useState({});
  const historyRef = useRef({});

  const ingest = useCallback((response) => {
    const next = { ...historyRef.current };
    for (const port of response.ports) {
      for (const queue of port.queues) {
        for (const field of ['tx_packets', 'dropped', 'wred_dropped', 'ecn_marked']) {
          const key = `${port.port}/${queue.queue}/${field}`;
          const series = [...(next[key] || []), queue[field]];
          next[key] = series.slice(-HISTORY);
        }
      }
    }
    historyRef.current = next;
    setHistory(next);
    setState(response);
  }, []);

  const refresh = useCallback(() => {
    api('/api/qos/ports')
      .then(ingest)
      .catch((e) => setError(e.message));
  }, [ingest]);
  useEffect(refresh, [refresh]);

  // The counters are live, so the grid keeps polling while it is open.
  useEffect(() => {
    const timer = setInterval(refresh, 5000);
    return () => clearInterval(timer);
  }, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setBulk(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const clearPort = async (port) => {
    setError(null);
    try {
      const result = await api('/api/qos/ports/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: [port.port] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  // The grid mirrors `show qos interfaces`: configured ports only, with
  // a Port-Channel member folded into its Po row.
  const rows = state
    ? state.ports.filter((port) => port.configured && !port.via_port_channel)
    : [];
  const meta = state || {};

  const queueList = (port, pick) => {
    const queues = port.queues.filter(pick).map((q) => q.queue).sort((a, b) => b - a);
    return queues.length === 0 ? '—' : queues.join(', ');
  };

  return (
    <Shell>
      <div className="page-header">
        <h2>QoS Interfaces</h2>
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
        <Datagrid
          selectable
          expandable
          rowKey={(r) => r.port}
          onRefresh={refresh}
          placeholder="No ports carry QoS configuration."
          footerText={`${rows.length} configured · ${state.default_ports} ports with default QoS configuration`}
          actionBar={({ selected }) => (
            <Button variant="outline" sm icon="layers" disabled={selected.size === 0}
              onClick={() => setBulk([...selected])}>
              Apply queue template
            </Button>
          )}
          renderDetail={(r) => <QueueTable port={r} history={history} />}
          columns={[
            {
              key: 'port', label: 'Port', sortable: true,
              render: (r) => <span className="cell-mono">{shortName(r.port)}</span>,
            },
            {
              key: 'trust', label: 'Trust', sortable: true,
              render: (r) => (r.trust === 'untrusted'
                ? <Label>{r.trust}</Label>
                : <Label accent>{r.trust}</Label>),
            },
            {
              key: 'default_tc', label: 'Def-TC',
              render: (r) => <span className="cell-mono">{r.default_tc}</span>,
            },
            {
              key: 'strict', label: 'Strict Qs',
              render: (r) => (
                <span className="cell-mono">{queueList(r, (q) => q.mode === 'strict')}</span>
              ),
            },
            {
              key: 'shaper', label: 'Shaper',
              render: (r) => (
                <span className="cell-mono">{displayRate(r.shape_bps) || '—'}</span>
              ),
            },
            {
              key: 'wred', label: 'WRED Qs',
              render: (r) => (
                <span className="cell-mono">{queueList(r, (q) => !!q.wred_profile)}</span>
              ),
            },
            {
              key: 'actions', label: '', width: 80,
              render: (r) => (
                <span style={{ display: 'inline-flex', gap: 2 }}>
                  <Button variant="link-neutral" sm icon="pencil"
                    aria-label={`Edit QoS on ${r.port}`}
                    onClick={() => setModal(r)} />
                  <Button variant="link-neutral" sm icon="undo"
                    aria-label={`Reset QoS on ${r.port}`}
                    onClick={() => clearPort(r)} />
                </span>
              ),
            },
          ]}
          rows={rows}
        />
      )}
      <PortModal open={!!modal} port={modal} meta={meta}
        onClose={() => setModal(null)} onSaved={onSaved} />
      <BulkModal open={!!bulk} ports={bulk || []} meta={meta}
        onClose={() => setBulk(null)} onSaved={onSaved} />
    </Shell>
  );
}
