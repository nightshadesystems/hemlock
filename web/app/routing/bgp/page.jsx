'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Card, CardBlock, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { Checkbox, FormField, Input, Textarea } from '@/components/ds/forms';

const REDISTRIBUTE = ['connected', 'static', 'ospf'];

function SettingsModal({ open, config, onClose, onSaved }) {
  const [asNumber, setAsNumber] = useState('');
  const [routerId, setRouterId] = useState('');
  const [maxPaths, setMaxPaths] = useState('');
  const [networks, setNetworks] = useState('');
  const [redistribute, setRedistribute] = useState([]);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setAsNumber(config?.as_number || '');
    setRouterId(config?.router_id || '');
    setMaxPaths(config?.maximum_paths || '');
    setNetworks((config?.networks || []).join('\n'));
    setRedistribute(config?.redistribute || []);
    setError(null);
    setBusy(false);
  }, [open, config]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    const body = {
      as_number: parseInt(asNumber, 10) || 0,
      router_id: routerId.trim(),
      networks: networks.split(/\s+/).map((s) => s.trim()).filter(Boolean),
      redistribute,
    };
    if (maxPaths.toString().trim()) body.maximum_paths = parseInt(maxPaths, 10);
    try {
      onSaved(await api('/api/bgp/edit', { method: 'POST', body: JSON.stringify(body) }));
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title="BGP Settings" size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy}
            disabled={busy || !asNumber.toString().trim()}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="AS Number" required htmlFor="bgp-as" helper="asplain, 1..4294967295">
          <Input id="bgp-as" className="mono" value={asNumber}
            onChange={(e) => setAsNumber(e.target.value)} style={{ maxWidth: 160 }} />
        </FormField>
        <FormField label="Router ID" htmlFor="bgp-rid" helper="Empty derives from routing router-id">
          <Input id="bgp-rid" className="mono" value={routerId}
            onChange={(e) => setRouterId(e.target.value)} style={{ maxWidth: 180 }} />
        </FormField>
        <FormField label="Maximum Paths" htmlFor="bgp-mp" helper="1..8 (ECMP width)">
          <Input id="bgp-mp" className="mono" value={maxPaths}
            onChange={(e) => setMaxPaths(e.target.value)} style={{ maxWidth: 100 }} />
        </FormField>
        <FormField label="Networks" htmlFor="bgp-nets" helper="One prefix per line">
          <Textarea id="bgp-nets" className="mono" rows={3} value={networks}
            onChange={(e) => setNetworks(e.target.value)} style={{ maxWidth: 240 }} />
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

function NeighborModal({ open, neighbor, onClose, onSaved }) {
  const editing = !!neighbor;
  const [ip, setIp] = useState('');
  const [remoteAs, setRemoteAs] = useState('');
  const [description, setDescription] = useState('');
  const [shutdown, setShutdown] = useState(false);
  const [multihop, setMultihop] = useState('');
  const [nextHopSelf, setNextHopSelf] = useState(false);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setIp(editing ? neighbor.ip : '');
    setRemoteAs(editing ? neighbor.remote_as || '' : '');
    setDescription(editing ? neighbor.description || '' : '');
    setShutdown(editing ? !!neighbor.shutdown : false);
    setMultihop(editing ? neighbor.ebgp_multihop || '' : '');
    setNextHopSelf(editing ? !!neighbor.next_hop_self : false);
    setError(null);
    setBusy(false);
  }, [open, editing, neighbor]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    const set = {
      ip: ip.trim(),
      remote_as: parseInt(remoteAs, 10) || 0,
      description: description.trim(),
      shutdown,
      next_hop_self: nextHopSelf,
    };
    if (multihop.toString().trim()) set.ebgp_multihop = parseInt(multihop, 10);
    try {
      onSaved(await api('/api/bgp/edit', {
        method: 'POST',
        body: JSON.stringify({ set_neighbors: [set] }),
      }));
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title={editing ? `Edit Neighbor ${neighbor.ip}` : 'New Neighbor'}
      size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy}
            disabled={busy || !ip.trim() || !remoteAs.toString().trim()}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Neighbor Address" required htmlFor="nbr-ip">
          <Input id="nbr-ip" className="mono" value={ip} disabled={editing} autoFocus={!editing}
            onChange={(e) => setIp(e.target.value)} style={{ maxWidth: 200 }} />
        </FormField>
        <FormField label="Remote AS" required htmlFor="nbr-as">
          <Input id="nbr-as" className="mono" value={remoteAs}
            onChange={(e) => setRemoteAs(e.target.value)} style={{ maxWidth: 160 }} />
        </FormField>
        <FormField label="Description" htmlFor="nbr-desc">
          <Input id="nbr-desc" value={description}
            onChange={(e) => setDescription(e.target.value)} style={{ maxWidth: 'none' }} />
        </FormField>
        <FormField label="eBGP Multihop" htmlFor="nbr-hop" helper="1..255; empty = off">
          <Input id="nbr-hop" className="mono" value={multihop}
            onChange={(e) => setMultihop(e.target.value)} style={{ maxWidth: 100 }} />
        </FormField>
        <FormField>
          <Checkbox label="Shutdown" checked={shutdown}
            onChange={(e) => setShutdown(e.target.checked)} />
          <Checkbox label="Next-hop-self" checked={nextHopSelf}
            onChange={(e) => setNextHopSelf(e.target.checked)} />
        </FormField>
      </div>
    </Modal>
  );
}

export default function BgpPage() {
  const [data, setData] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api('/api/bgp')
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

  const removeNeighbor = async (ip) => {
    setError(null);
    try {
      onSaved(await api('/api/bgp/edit', {
        method: 'POST',
        body: JSON.stringify({ delete_neighbors: [ip] }),
      }));
    } catch (err) {
      setError(err.message);
    }
  };

  const config = data?.config;
  const live = data?.state;
  const neighbors = (config?.neighbors || []).map((neighbor) => ({
    ...neighbor,
    live: (live?.peers || []).find((peer) => peer.ip === neighbor.ip),
  }));

  return (
    <Shell>
      <div className="page-header"><h2>BGP</h2></div>
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
              <CardBlock title="Local AS" text={config?.as_number || String(live?.as_number ?? '—')} />
              <CardBlock title="Router ID" text={live?.router_id || config?.router_id || 'derived'} />
              <CardBlock title="Networks" text={(config?.networks || []).join(', ') || '—'} />
              <CardBlock title="Redistribute" text={(config?.redistribute || []).join(', ') || '—'} />
            </div>
          </Card>
          <Card
            header={
              <div style={{ display: 'flex', alignItems: 'center', width: '100%', gap: 12 }}>
                <span className="card-title" style={{ marginRight: 'auto' }}>Neighbors</span>
                <Button sm variant="outline" icon="plus" onClick={() => setModal({ kind: 'neighbor' })}>
                  New Neighbor
                </Button>
              </div>
            }
          >
            <Datagrid
              rowKey={(r) => r.ip}
              onRefresh={refresh}
              columns={[
                { key: 'ip', label: 'Neighbor', render: (r) => <span className="cell-mono">{r.ip}</span> },
                { key: 'remote_as', label: 'Remote AS', render: (r) => <span className="cell-mono">{r.remote_as}</span> },
                {
                  key: 'state', label: 'State', width: 120,
                  render: (r) =>
                    r.shutdown ? (
                      <Label>Shutdown</Label>
                    ) : r.live ? (
                      <Label status={r.live.state === 'Established' ? 'success' : 'warning'}>
                        {r.live.state}
                      </Label>
                    ) : (
                      <span className="dim">—</span>
                    ),
                },
                {
                  key: 'uptime', label: 'Up/Down', width: 100,
                  render: (r) => <span className="cell-mono">{r.live?.up_down || '—'}</span>,
                },
                {
                  key: 'pfx', label: 'PfxRcd', width: 90,
                  render: (r) => (
                    <span className="cell-mono">
                      {r.live && r.live.pfx_rcvd >= 0 ? r.live.pfx_rcvd : '—'}
                    </span>
                  ),
                },
                { key: 'description', label: 'Description', render: (r) => r.description || <span className="dim">—</span> },
                {
                  key: 'actions', label: '', width: 80,
                  render: (r) => (
                    <span style={{ display: 'inline-flex', gap: 2 }}>
                      <Button variant="link-neutral" sm icon="pencil" aria-label={`Edit ${r.ip}`}
                        onClick={() => setModal({ kind: 'neighbor', neighbor: r })} />
                      <Button variant="link-neutral" sm icon="trash" aria-label={`Delete ${r.ip}`}
                        onClick={() => removeNeighbor(r.ip)} />
                    </span>
                  ),
                },
              ]}
              rows={neighbors}
              placeholder="No neighbors configured."
            />
          </Card>
        </>
      )}
      <SettingsModal open={modal?.kind === 'settings'} config={config}
        onClose={() => setModal(null)} onSaved={onSaved} />
      <NeighborModal open={modal?.kind === 'neighbor'} neighbor={modal?.neighbor}
        onClose={() => setModal(null)} onSaved={onSaved} />
    </Shell>
  );
}
