'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Card, CardBlock, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { Checkbox, FormField, Input, Textarea } from '@/components/ds/forms';

const REDISTRIBUTE = ['connected', 'static', 'bgp'];

function SettingsModal({ open, config, onClose, onSaved }) {
  const [routerId, setRouterId] = useState('');
  const [maxPaths, setMaxPaths] = useState('');
  const [passive, setPassive] = useState('');
  const [redistribute, setRedistribute] = useState([]);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setRouterId(config?.router_id || '');
    setMaxPaths(config?.maximum_paths || '');
    setPassive((config?.passive_interfaces || []).join(', '));
    setRedistribute(config?.redistribute || []);
    setError(null);
    setBusy(false);
  }, [open, config]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    const body = {
      router_id: routerId.trim(),
      passive_interfaces: passive.split(',').map((s) => s.trim()).filter(Boolean),
      redistribute,
    };
    if (maxPaths.toString().trim()) body.maximum_paths = parseInt(maxPaths, 10);
    try {
      onSaved(await api('/api/ospf/edit', { method: 'POST', body: JSON.stringify(body) }));
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title="OSPF Settings" size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Router ID" htmlFor="ospf-rid" helper="Empty derives from routing router-id">
          <Input id="ospf-rid" className="mono" value={routerId}
            onChange={(e) => setRouterId(e.target.value)} style={{ maxWidth: 180 }} />
        </FormField>
        <FormField label="Maximum Paths" htmlFor="ospf-mp" helper="1..8 (ECMP width)">
          <Input id="ospf-mp" className="mono" value={maxPaths}
            onChange={(e) => setMaxPaths(e.target.value)} style={{ maxWidth: 100 }} />
        </FormField>
        <FormField label="Passive Interfaces" htmlFor="ospf-passive" helper="Comma-separated">
          <Input id="ospf-passive" className="mono" value={passive}
            onChange={(e) => setPassive(e.target.value)} style={{ maxWidth: 'none' }} />
        </FormField>
        <FormField label="Redistribute">
          {REDISTRIBUTE.map((source) => (
            <Checkbox key={source} label={source} checked={redistribute.includes(source)}
              onChange={(e) =>
                setRedistribute(e.target.checked
                  ? [...redistribute, source]
                  : redistribute.filter((s) => s !== source))
              } />
          ))}
        </FormField>
      </div>
    </Modal>
  );
}

function AreaModal({ open, area, areas, onClose, onSaved }) {
  const editing = !!area;
  const [id, setId] = useState('');
  const [networks, setNetworks] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setId(editing ? area.id : '');
    setNetworks(editing ? (area.networks || []).join('\n') : '');
    setError(null);
    setBusy(false);
  }, [open, editing, area]);

  const submit = async (remove) => {
    setBusy(true);
    setError(null);
    const others = (areas || []).filter((a) => a.id !== (editing ? area.id : id.trim()));
    const next = remove
      ? others
      : [...others, {
          id: id.trim(),
          networks: networks.split(/\s+/).map((s) => s.trim()).filter(Boolean),
        }];
    try {
      onSaved(await api('/api/ospf/edit', {
        method: 'POST',
        body: JSON.stringify({ areas: next }),
      }));
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title={editing ? `Edit Area ${area.id}` : 'New Area'} size="sm" onClose={onClose}
      footer={
        <>
          {editing && (
            <Button variant="danger-outline" onClick={() => submit(true)} disabled={busy}>Delete</Button>
          )}
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={() => submit(false)} loading={busy}
            disabled={busy || !id.trim()}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Area" required htmlFor="area-id" helper="Dotted or integer">
          <Input id="area-id" className="mono" value={id} disabled={editing}
            onChange={(e) => setId(e.target.value)} style={{ maxWidth: 160 }} />
        </FormField>
        <FormField label="Networks" required htmlFor="area-nets" helper="One prefix per line">
          <Textarea id="area-nets" className="mono" rows={3} value={networks}
            onChange={(e) => setNetworks(e.target.value)} style={{ maxWidth: 240 }} />
        </FormField>
      </div>
    </Modal>
  );
}

export default function OspfPage() {
  const [data, setData] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null); // {kind:'settings'} | {kind:'area', area?}

  const refresh = useCallback(() => {
    api('/api/ospf')
      .then((r) => {
        setData(r);
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

  const config = data?.config;
  const live = data?.state;

  return (
    <Shell>
      <div className="page-header"><h2>OSPF</h2></div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!data && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {data && (
        <>
          <Card
            header={
              <div style={{ display: 'flex', alignItems: 'center', width: '100%', gap: 12 }}>
                <span className="card-title" style={{ marginRight: 'auto' }}>Process</span>
                {live ? <Label status="success">Running</Label> : <Label>Not Running</Label>}
                <Button sm variant="outline" icon="cog" onClick={() => setModal({ kind: 'settings' })}>
                  Settings
                </Button>
              </div>
            }
            style={{ marginBottom: 16 }}
          >
            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(4, 1fr)', gap: 12 }}>
              <CardBlock title="Router ID"
                text={live?.router_id || config?.router_id || 'derived'} />
              <CardBlock title="SPF Runs" text={String(live?.spf_runs ?? '—')} />
              <CardBlock title="Maximum Paths" text={config?.maximum_paths || '4'} />
              <CardBlock title="Redistribute"
                text={(config?.redistribute || []).join(', ') || '—'} />
            </div>
          </Card>
          <Card
            header={
              <div style={{ display: 'flex', alignItems: 'center', width: '100%', gap: 12 }}>
                <span className="card-title" style={{ marginRight: 'auto' }}>Areas</span>
                <Button sm variant="outline" icon="plus" onClick={() => setModal({ kind: 'area' })}>
                  New Area
                </Button>
              </div>
            }
            style={{ marginBottom: 16 }}
          >
            <Datagrid
              rowKey={(r) => r.id}
              columns={[
                { key: 'id', label: 'Area', render: (r) => <span className="cell-mono">{r.id}</span> },
                {
                  key: 'networks', label: 'Networks',
                  render: (r) => <span className="cell-mono">{(r.networks || []).join(', ')}</span>,
                },
                {
                  key: 'actions', label: '', width: 50,
                  render: (r) => (
                    <Button variant="link-neutral" sm icon="pencil" aria-label={`Edit area ${r.id}`}
                      onClick={() => setModal({ kind: 'area', area: r })} />
                  ),
                },
              ]}
              rows={config?.areas || []}
              placeholder="No areas configured."
            />
          </Card>
          <Card header={<span className="card-title">Neighbors</span>}>
            <Datagrid
              rowKey={(r) => `${r.router_id}-${r.interface}`}
              onRefresh={refresh}
              columns={[
                { key: 'router_id', label: 'Neighbor ID', render: (r) => <span className="cell-mono">{r.router_id}</span> },
                {
                  key: 'state', label: 'State', width: 110,
                  render: (r) => (
                    <Label status={r.state === 'Full' ? 'success' : 'warning'}>{r.state}</Label>
                  ),
                },
                { key: 'address', label: 'Address', render: (r) => <span className="cell-mono">{r.address}</span> },
                { key: 'interface', label: 'Interface', render: (r) => <span className="cell-mono">{r.interface}</span> },
                {
                  key: 'dead', label: 'Dead Time', width: 110,
                  render: (r) => <span className="cell-mono">{Math.round(r.dead_time_msecs / 1000)}s</span>,
                },
              ]}
              rows={live?.neighbors || []}
              placeholder={live ? 'No neighbors.' : 'OSPF is not running.'}
            />
          </Card>
        </>
      )}
      <SettingsModal open={modal?.kind === 'settings'} config={config}
        onClose={() => setModal(null)} onSaved={onSaved} />
      <AreaModal open={modal?.kind === 'area'} area={modal?.area} areas={config?.areas}
        onClose={() => setModal(null)} onSaved={onSaved} />
    </Shell>
  );
}
