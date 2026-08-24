'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button, ButtonGroup } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input } from '@/components/ds/forms';

function StaticModal({ open, onClose, onSaved }) {
  const [ip, setIp] = useState('');
  const [iface, setIface] = useState('');
  const [mac, setMac] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setIp('');
    setIface('');
    setMac('');
    setError(null);
    setBusy(false);
  }, [open]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/arp/edit', {
        method: 'POST',
        body: JSON.stringify({ set: [{ ip: ip.trim(), interface: iface.trim(), mac: mac.trim() }] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      title="New Static Entry"
      size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy}
            disabled={busy || !ip.trim() || !iface.trim() || !mac.trim()}>
            Commit
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Address" required htmlFor="arp-ip" helper="IPv4 or IPv6">
          <Input id="arp-ip" className="mono" value={ip} autoFocus
            onChange={(e) => setIp(e.target.value)} style={{ maxWidth: 240 }} />
        </FormField>
        <FormField label="Interface" required htmlFor="arp-iface"
          helper="An L3 interface, e.g. Vlan99 or Ethernet48">
          <Input id="arp-iface" className="mono" value={iface}
            onChange={(e) => setIface(e.target.value)} style={{ maxWidth: 180 }} />
        </FormField>
        <FormField label="MAC address" required htmlFor="arp-mac" helper="Unicast; colon form">
          <Input id="arp-mac" className="mono" value={mac}
            onChange={(e) => setMac(e.target.value)} style={{ maxWidth: 200 }} />
        </FormField>
      </div>
    </Modal>
  );
}

export default function ArpPage() {
  const [family, setFamily] = useState('v4');
  const [neighbors, setNeighbors] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(false);

  const refresh = useCallback(() => {
    api(`/api/arp?family=${family}`)
      .then((r) => {
        setNeighbors(r.neighbors);
        setError(null);
      })
      .catch((e) => setError(e.message));
  }, [family]);
  useEffect(refresh, [refresh]);

  const onSaved = (result) => {
    setModal(false);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const flush = async (ip) => {
    setError(null);
    try {
      await api('/api/arp/flush', { method: 'POST', body: JSON.stringify({ ip: ip || '' }) });
      setApplied([ip ? `Flushed ${ip}.` : 'Flushed dynamic entries.']);
      refresh();
    } catch (err) {
      setError(err.message);
    }
  };

  const removeStatic = async (ip) => {
    setError(null);
    try {
      const result = await api('/api/arp/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: [ip] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  return (
    <Shell>
      <div className="page-header" style={{ display: 'flex', alignItems: 'center', gap: 16 }}>
        <h2 style={{ marginRight: 'auto' }}>ARP / ND</h2>
        <ButtonGroup>
          <Button sm variant={family === 'v4' ? 'primary' : 'outline'}
            onClick={() => setFamily('v4')}>ARP</Button>
          <Button sm variant={family === 'v6' ? 'primary' : 'outline'}
            onClick={() => setFamily('v6')}>ND</Button>
        </ButtonGroup>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!neighbors && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {neighbors && (
        <Datagrid
          rowKey={(r) => r.ip}
          onRefresh={refresh}
          actionBar={() => (
            <>
              <Button variant="primary" sm icon="plus" onClick={() => setModal(true)}>
                New Static Entry
              </Button>
              <Button variant="danger-outline" sm onClick={() => flush('')}>
                Flush Dynamic
              </Button>
            </>
          )}
          columns={[
            {
              key: 'ip', label: 'Address', sortable: true,
              render: (r) => <span className="cell-mono">{r.ip}</span>,
            },
            {
              key: 'age', label: 'Age (sec)', width: 100,
              render: (r) =>
                r.is_static
                  ? <span className="dim">—</span>
                  : <span className="cell-mono">{r.age_secs ?? ''}</span>,
            },
            {
              key: 'mac', label: 'Hardware Addr',
              render: (r) => <span className="cell-mono">{r.mac || <span className="dim">unresolved</span>}</span>,
            },
            {
              key: 'interface', label: 'Interface',
              render: (r) => <span className="cell-mono">{r.interface}</span>,
            },
            {
              key: 'kind', label: '', width: 90,
              render: (r) => (r.is_static ? <Label status="info">Static</Label> : null),
            },
            {
              key: 'actions', label: '', width: 50,
              render: (r) => (
                <Button variant="link-neutral" sm icon="trash"
                  aria-label={`Remove ${r.ip}`}
                  onClick={() => (r.is_static ? removeStatic(r.ip) : flush(r.ip))} />
              ),
            },
          ]}
          rows={neighbors}
          placeholder="No neighbor entries."
        />
      )}
      <StaticModal open={modal} onClose={() => setModal(false)} onSaved={onSaved} />
    </Shell>
  );
}
