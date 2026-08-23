'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, shortName, compareNames, parseVlanList } from '@/lib/api';
import { Alert, Badge, Card, CardBlock, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Select, Checkbox } from '@/components/ds/forms';

const STATE_BADGE = {
  forwarding: 'success',
  learning: 'warning',
  discarding: 'danger',
};

/// Global bridge settings (mode, priority, timers, MST region).
function GlobalModal({ open, stp, onClose, onSaved }) {
  const [mode, setMode] = useState('mstp');
  const [priority, setPriority] = useState('32768');
  const [hello, setHello] = useState('2');
  const [maxAge, setMaxAge] = useState('20');
  const [forward, setForward] = useState('15');
  const [mstName, setMstName] = useState('');
  const [mstRevision, setMstRevision] = useState('0');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open || !stp) return;
    setMode(stp.mode || 'mstp');
    setPriority(String(stp.bridge_priority));
    setHello(String(stp.hello_time));
    setMaxAge(String(stp.max_age));
    setForward(String(stp.forward_time));
    setMstName(stp.mst_name || '');
    setMstRevision(String(stp.mst_revision));
    setError(null);
    setBusy(false);
  }, [open, stp]);

  const submit = async () => {
    const prio = parseInt(priority, 10);
    if (!Number.isInteger(prio) || prio < 0 || prio > 61440 || prio % 4096 !== 0) {
      setError('Priority must be 0..61440 in steps of 4096.');
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/spanning-tree/edit', {
        method: 'POST',
        body: JSON.stringify({
          mode,
          priority: prio,
          hello_time: parseInt(hello, 10),
          max_age: parseInt(maxAge, 10),
          forward_time: parseInt(forward, 10),
          mst_name: mstName,
          mst_revision: parseInt(mstRevision, 10) || 0,
        }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title="Spanning tree settings" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Mode" htmlFor="stp-mode">
          <Select id="stp-mode" value={mode} onChange={(e) => setMode(e.target.value)}
            options={[
              { value: 'mstp', label: 'MSTP' },
              { value: 'rstp', label: 'RSTP' },
              { value: 'none', label: 'Disabled' },
            ]} />
        </FormField>
        <FormField label="Bridge priority" htmlFor="stp-priority" helper="0..61440, steps of 4096">
          <Input id="stp-priority" className="mono" value={priority}
            onChange={(e) => setPriority(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <FormField label="Hello time" htmlFor="stp-hello" helper="1..10 s">
          <Input id="stp-hello" className="mono" value={hello}
            onChange={(e) => setHello(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <FormField label="Max age" htmlFor="stp-max-age" helper="6..40 s">
          <Input id="stp-max-age" className="mono" value={maxAge}
            onChange={(e) => setMaxAge(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <FormField label="Forward delay" htmlFor="stp-forward" helper="4..30 s">
          <Input id="stp-forward" className="mono" value={forward}
            onChange={(e) => setForward(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <FormField label="MST region name" htmlFor="stp-mst-name">
          <Input id="stp-mst-name" value={mstName}
            onChange={(e) => setMstName(e.target.value)} />
        </FormField>
        <FormField label="MST revision" htmlFor="stp-mst-revision">
          <Input id="stp-mst-revision" className="mono" value={mstRevision}
            onChange={(e) => setMstRevision(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
      </div>
    </Modal>
  );
}

/// The MST instance-to-VLAN mapping editor (replaces the whole map).
function InstancesModal({ open, stp, onClose, onSaved }) {
  const [rows, setRows] = useState([]);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open || !stp) return;
    setRows(stp.instances.map((m) => ({
      instance: String(m.instance),
      vlans: m.vlans.join(','),
    })));
    setError(null);
    setBusy(false);
  }, [open, stp]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const instances = [];
      const seen = new Set();
      for (const row of rows) {
        if (!row.instance && !row.vlans) continue;
        const instance = parseInt(row.instance, 10);
        if (!Number.isInteger(instance) || instance < 1 || instance > 15) {
          throw new Error(`Instance must be 1..15 (got "${row.instance}").`);
        }
        const vlans = parseVlanList(row.vlans);
        for (const vlan of vlans) {
          if (seen.has(vlan)) throw new Error(`VLAN ${vlan} is mapped twice.`);
          seen.add(vlan);
        }
        instances.push({ instance, vlans });
      }
      const result = await api('/api/spanning-tree/edit', {
        method: 'POST',
        body: JSON.stringify({ instances }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title="MST instances" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <p className="clr-secondary" style={{ marginBottom: 12 }}>
        Instance 0 is implicit and carries every unmapped VLAN. Mappings here replace the
        current set.
      </p>
      <div className="clr-form-compact">
        {rows.map((row, i) => (
          <div key={i} style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
            <Input className="mono" value={row.instance} placeholder="1"
              aria-label={`Instance ${i + 1}`}
              onChange={(e) => setRows(rows.map((r, n) => (n === i ? { ...r, instance: e.target.value } : r)))}
              style={{ maxWidth: 80 }} />
            <Input className="mono" value={row.vlans} placeholder="10,20,30"
              aria-label={`Instance ${i + 1} VLANs`}
              onChange={(e) => setRows(rows.map((r, n) => (n === i ? { ...r, vlans: e.target.value } : r)))} />
            <Button variant="link-neutral" sm icon="trash" aria-label="Remove instance"
              onClick={() => setRows(rows.filter((_, n) => n !== i))} />
          </div>
        ))}
        <Button variant="outline" sm icon="plus"
          onClick={() => setRows([...rows, { instance: '', vlans: '' }])}>
          Add instance
        </Button>
      </div>
    </Modal>
  );
}

/// One port's STP settings.
function PortModal({ open, port, onClose, onSaved }) {
  const [portfast, setPortfast] = useState(false);
  const [bpduguard, setBpduguard] = useState(false);
  const [cost, setCost] = useState('');
  const [priority, setPriority] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open || !port) return;
    setPortfast(port.portfast);
    setBpduguard(port.bpduguard);
    setCost('');
    setPriority('');
    setError(null);
    setBusy(false);
  }, [open, port]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const body = {
        ports: [{
          name: port.port,
          portfast,
          bpduguard,
          ...(cost !== '' ? { cost: parseInt(cost, 10) } : {}),
          ...(priority !== '' ? { port_priority: parseInt(priority, 10) } : {}),
        }],
      };
      const result = await api('/api/spanning-tree/edit', {
        method: 'POST',
        body: JSON.stringify(body),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  if (!port) return null;
  return (
    <Modal open={open} title={`Spanning tree · ${port.port}`} size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <Checkbox label="Portfast (edge port)" checked={portfast}
          onChange={(e) => setPortfast(e.target.checked)} />
        <Checkbox label="BPDU guard" checked={bpduguard}
          onChange={(e) => setBpduguard(e.target.checked)} />
        <FormField label="Cost" htmlFor="stp-port-cost"
          helper={`Current ${port.cost}; 0 restores the speed default`}>
          <Input id="stp-port-cost" className="mono" value={cost} placeholder="unchanged"
            onChange={(e) => setCost(e.target.value)} style={{ maxWidth: 140 }} />
        </FormField>
        <FormField label="Port priority" htmlFor="stp-port-priority"
          helper={`Current ${port.priority}; steps of 16, 0 restores 128`}>
          <Input id="stp-port-priority" className="mono" value={priority} placeholder="unchanged"
            onChange={(e) => setPriority(e.target.value)} style={{ maxWidth: 140 }} />
        </FormField>
      </div>
    </Modal>
  );
}

export default function SpanningTreePage() {
  const [stp, setStp] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api('/api/spanning-tree').then(setStp).catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const reenable = async (port) => {
    try {
      await api('/api/spanning-tree/clear-errdisable', {
        method: 'POST',
        body: JSON.stringify({ port }),
      });
      setApplied([`${port} re-enabled.`]);
      refresh();
    } catch (err) {
      setError(err.message);
    }
  };

  const errdisabled = stp ? stp.ports.filter((p) => p.errdisabled) : [];
  const ports = stp
    ? [...stp.ports].sort((a, b) => compareNames(a.port, b.port))
    : [];

  return (
    <Shell>
      <div className="page-header">
        <h2>Spanning Tree</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {errdisabled.length > 0 && (
        <Alert status="warning" style={{ marginBottom: 16 }}
          actions={errdisabled.map((p) => (
            <Button key={p.port} variant="link" sm onClick={() => reenable(p.port)}>
              Re-enable {shortName(p.port)}
            </Button>
          ))}>
          BPDU guard errdisabled: {errdisabled.map((p) => shortName(p.port)).join(', ')}
        </Alert>
      )}
      {!stp && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {stp && (
        <>
          <Card
            header={
              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                <span>
                  Bridge{' '}
                  {stp.is_root
                    ? <Label status="success">root bridge</Label>
                    : <Label>root is {stp.root_priority},{stp.root_mac}</Label>}
                </span>
                <Button variant="outline" sm icon="pencil" onClick={() => setModal({ kind: 'global' })}>
                  Settings
                </Button>
              </div>
            }
            style={{ marginBottom: 16 }}
          >
            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(4, minmax(0,1fr))', gap: 16 }}>
              <CardBlock title="Mode" text={stp.mode} />
              <CardBlock title="Bridge ID"
                text={`${stp.bridge_priority}, ${stp.bridge_mac}`} />
              <CardBlock title="Timers"
                text={`hello ${stp.hello_time}s · max age ${stp.max_age}s · fwd ${stp.forward_time}s`} />
              <CardBlock title="Topology changes"
                text={`${stp.topology_changes}${stp.last_tc_port ? ` (last: ${stp.last_tc_port})` : ''}`} />
            </div>
          </Card>

          <Card
            header={
              <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
                <span>
                  MST region{' '}
                  <span className="mono">
                    {stp.mst_name ? `${stp.mst_name} (rev ${stp.mst_revision})` : 'unnamed'}
                  </span>
                </span>
                <Button variant="outline" sm icon="pencil" onClick={() => setModal({ kind: 'instances' })}>
                  Edit instances
                </Button>
              </div>
            }
            style={{ marginBottom: 16 }}
          >
            <table className="table">
              <thead>
                <tr><th style={{ width: 120 }}>Instance</th><th>VLANs mapped</th></tr>
              </thead>
              <tbody>
                <tr>
                  <td className="cell-mono">0</td>
                  <td className="dim">all unmapped VLANs</td>
                </tr>
                {stp.instances.map((m) => (
                  <tr key={m.instance}>
                    <td className="cell-mono">{m.instance}</td>
                    <td className="cell-mono">{m.vlans.join(', ')}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </Card>

          <Datagrid
            rowKey={(r) => r.port}
            columns={[
              {
                key: 'port', label: 'Port', sortable: true,
                render: (r) => <span className="cell-mono">{shortName(r.port)}</span>,
              },
              { key: 'role', label: 'Role', render: (r) => r.role },
              {
                key: 'state', label: 'State',
                render: (r) => r.errdisabled
                  ? <Badge status="danger">errdisabled</Badge>
                  : <Badge status={STATE_BADGE[r.state]}>{r.state}</Badge>,
              },
              { key: 'cost', label: 'Cost', render: (r) => <span className="cell-mono">{r.cost}</span> },
              { key: 'priority', label: 'Priority', render: (r) => <span className="cell-mono">{r.priority}</span> },
              {
                key: 'flags', label: 'Features',
                render: (r) => (
                  <span style={{ display: 'inline-flex', gap: 6 }}>
                    {r.portfast && <Label>portfast</Label>}
                    {r.bpduguard && <Label>bpduguard</Label>}
                    {!r.portfast && !r.bpduguard && <span className="dim">—</span>}
                  </span>
                ),
              },
              {
                key: 'actions', label: '', width: 60,
                render: (r) => (
                  <Button variant="link-neutral" sm icon="pencil" aria-label={`Edit ${r.port}`}
                    onClick={() => setModal({ kind: 'port', port: r })} />
                ),
              },
            ]}
            rows={ports}
            placeholder="No ports participating (all links down)."
          />
        </>
      )}
      <GlobalModal open={!!modal && modal.kind === 'global'} stp={stp}
        onClose={() => setModal(null)} onSaved={onSaved} />
      <InstancesModal open={!!modal && modal.kind === 'instances'} stp={stp}
        onClose={() => setModal(null)} onSaved={onSaved} />
      <PortModal open={!!modal && modal.kind === 'port'}
        port={modal && modal.kind === 'port' ? modal.port : null}
        onClose={() => setModal(null)} onSaved={onSaved} />
    </Shell>
  );
}
