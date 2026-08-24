'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, shortName, compareNames, parseVlanList } from '@/lib/api';
import { Alert, Badge, Card, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Select, SearchSelect, Checkbox } from '@/components/ds/forms';

const VALIDATE_CHECKS = ['src-mac', 'dst-mac', 'ip'];

function ConfigTab({ data, interfaces, onSaved, onError }) {
  const [dhcpVlans, setDhcpVlans] = useState('');
  const [arpVlans, setArpVlans] = useState('');
  const [validate, setValidate] = useState([]);
  const [busyDhcp, setBusyDhcp] = useState(false);
  const [busyArp, setBusyArp] = useState(false);
  const [trustIface, setTrustIface] = useState('');
  const [trustFeature, setTrustFeature] = useState('dhcp-snooping');

  useEffect(() => {
    setDhcpVlans(data.dhcp.vlans.join(','));
    setArpVlans(data.arp.vlans.join(','));
    setValidate(data.arp.validate);
  }, [data]);

  const commitDhcp = async () => {
    let vlans;
    try {
      vlans = dhcpVlans.trim() ? parseVlanList(dhcpVlans) : [];
    } catch (err) {
      onError(err.message);
      return;
    }
    setBusyDhcp(true);
    try {
      const result = await api('/api/snooping-sec/edit', {
        method: 'POST',
        body: JSON.stringify({ dhcp_vlans: vlans }),
      });
      onSaved(result);
    } catch (err) {
      onError(err.message);
    } finally {
      setBusyDhcp(false);
    }
  };

  const commitArp = async () => {
    let vlans;
    try {
      vlans = arpVlans.trim() ? parseVlanList(arpVlans) : [];
    } catch (err) {
      onError(err.message);
      return;
    }
    setBusyArp(true);
    try {
      const result = await api('/api/snooping-sec/edit', {
        method: 'POST',
        body: JSON.stringify({ arp_vlans: vlans, validate }),
      });
      onSaved(result);
    } catch (err) {
      onError(err.message);
    } finally {
      setBusyArp(false);
    }
  };

  const setTrust = async (iface, feature, trusted) => {
    try {
      const result = await api('/api/snooping-sec/edit', {
        method: 'POST',
        body: JSON.stringify({ trust_set: [{ interface: iface, feature, trusted }] }),
      });
      onSaved(result);
    } catch (err) {
      onError(err.message);
    }
  };

  const trustRows = [
    ...data.dhcp.trusted.map((iface) => ({ iface, feature: 'dhcp-snooping' })),
    ...data.arp.trusted.map((iface) => ({ iface, feature: 'arp-inspection' })),
  ].sort((a, b) => compareNames(a.iface, b.iface));

  return (
    <>
      <Card header="DHCP Snooping" style={{ marginBottom: 16 }}>
        <div className="clr-form-compact">
          <FormField label="VLANs" htmlFor="dhcp-vlans"
            helper="Comma/range list, e.g. 10,20,30-32; empty disables">
            <Input id="dhcp-vlans" className="mono" value={dhcpVlans}
              onChange={(e) => setDhcpVlans(e.target.value)} style={{ maxWidth: 320 }} />
          </FormField>
          <Button variant="primary" sm onClick={commitDhcp} loading={busyDhcp} disabled={busyDhcp}>
            Commit
          </Button>
        </div>
      </Card>

      <Card header="ARP Inspection" style={{ marginBottom: 16 }}>
        <div className="clr-form-compact">
          <FormField label="VLANs" htmlFor="arp-vlans"
            helper="Comma/range list; empty disables">
            <Input id="arp-vlans" className="mono" value={arpVlans}
              onChange={(e) => setArpVlans(e.target.value)} style={{ maxWidth: 320 }} />
          </FormField>
          <FormField label="Validate" helper="Extra checks on ARP payloads; src-mac is the default">
            <div style={{ display: 'flex', gap: 16 }}>
              {VALIDATE_CHECKS.map((check) => (
                <Checkbox key={check} label={check} checked={validate.includes(check)}
                  onChange={(e) => setValidate(e.target.checked
                    ? [...validate, check]
                    : validate.filter((c) => c !== check))} />
              ))}
            </div>
          </FormField>
          <Button variant="primary" sm onClick={commitArp} loading={busyArp} disabled={busyArp}>
            Commit
          </Button>
        </div>
      </Card>

      <Card header="Trusted Interfaces">
        <div style={{ display: 'flex', alignItems: 'flex-end', gap: 12, marginBottom: 12, flexWrap: 'wrap' }}>
          <FormField label="Interface">
            <SearchSelect options={interfaces.map((name) => ({ value: name, label: name }))}
              value={trustIface} onChange={setTrustIface} placeholder="Select interface…" />
          </FormField>
          <FormField label="Feature">
            <Select options={['dhcp-snooping', 'arp-inspection']} value={trustFeature}
              onChange={(e) => setTrustFeature(e.target.value)} />
          </FormField>
          <Button variant="primary" sm icon="plus" disabled={!trustIface}
            onClick={() => setTrust(trustIface, trustFeature, true)}>
            Trust
          </Button>
        </div>
        {trustRows.length === 0 && (
          <p className="dim" style={{ margin: 0 }}>
            No trusted interfaces — DHCP server replies and ARP traffic on uplinks will be dropped.
          </p>
        )}
        {trustRows.map((row) => (
          <div key={`${row.iface}-${row.feature}`}
            style={{ display: 'flex', alignItems: 'center', gap: 12, padding: '2px 0' }}>
            <span className="cell-mono">{shortName(row.iface)}</span>
            <Label>{row.feature}</Label>
            <Button variant="link-neutral" sm icon="trash"
              aria-label={`Untrust ${row.iface} for ${row.feature}`}
              onClick={() => setTrust(row.iface, row.feature, false)} />
          </div>
        ))}
      </Card>
    </>
  );
}

function BindingModal({ open, interfaces, onClose, onSaved }) {
  const [mac, setMac] = useState('');
  const [vlan, setVlan] = useState('');
  const [address, setAddress] = useState('');
  const [iface, setIface] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setMac('');
    setVlan('');
    setAddress('');
    setIface('');
    setError(null);
    setBusy(false);
  }, [open]);

  const submit = async () => {
    const parsed = parseInt(vlan, 10);
    if (!Number.isInteger(parsed) || parsed < 1 || parsed > 4094) {
      setError('VLAN id must be 1..4094.');
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/snooping-sec/edit', {
        method: 'POST',
        body: JSON.stringify({
          bindings_set: [{ mac: mac.trim(), vlan: parsed, address: address.trim(), interface: iface }],
        }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title="Add Static Binding" size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy}
            disabled={busy || !mac.trim() || !vlan.trim() || !address.trim() || !iface}>
            Commit
          </Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="MAC" required htmlFor="binding-mac" helper="Unicast, any common form">
          <Input id="binding-mac" className="mono" value={mac} autoFocus
            onChange={(e) => setMac(e.target.value)} style={{ maxWidth: 220 }} />
        </FormField>
        <FormField label="VLAN" required htmlFor="binding-vlan" helper="1..4094">
          <Input id="binding-vlan" className="mono" value={vlan}
            onChange={(e) => setVlan(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
        <FormField label="Address" required htmlFor="binding-address" helper="IPv4">
          <Input id="binding-address" className="mono" value={address}
            onChange={(e) => setAddress(e.target.value)} style={{ maxWidth: 220 }} />
        </FormField>
        <FormField label="Interface" required>
          <SearchSelect options={interfaces.map((name) => ({ value: name, label: name }))}
            value={iface} onChange={setIface} placeholder="Select interface…" />
        </FormField>
      </div>
    </Modal>
  );
}

function BindingsTab({ data, interfaces, onSaved, onError, setApplied, refresh }) {
  const [modal, setModal] = useState(false);

  const clear = async (mac) => {
    try {
      const result = await api('/api/snooping-sec/bindings/clear', {
        method: 'POST',
        body: JSON.stringify(mac ? { mac } : {}),
      });
      setApplied([`Cleared ${result.cleared} binding${result.cleared === 1 ? '' : 's'}.`]);
      refresh();
    } catch (err) {
      onError(err.message);
    }
  };

  const removeStatic = async (binding) => {
    try {
      const result = await api('/api/snooping-sec/edit', {
        method: 'POST',
        body: JSON.stringify({ bindings_delete: [{ mac: binding.mac, vlan: binding.vlan }] }),
      });
      onSaved(result);
    } catch (err) {
      onError(err.message);
    }
  };

  return (
    <>
      <Datagrid
        rowKey={(r) => `${r.mac}-${r.vlan}`}
        onRefresh={refresh}
        actionBar={() => (
          <>
            <Button variant="primary" sm icon="plus" onClick={() => setModal(true)}>
              Add Static Binding
            </Button>
            <Button variant="outline" sm onClick={() => clear(null)}>Clear Dynamic</Button>
          </>
        )}
        columns={[
          { key: 'mac', label: 'MAC', sortable: true, render: (r) => <span className="cell-mono">{r.mac}</span> },
          { key: 'address', label: 'IP Address', render: (r) => <span className="cell-mono">{r.address}</span> },
          {
            key: 'lease', label: 'Lease',
            render: (r) => r.lease_secs != null
              ? <span className="cell-mono">{r.lease_secs}s</span>
              : <span className="dim">-</span>,
          },
          {
            key: 'type', label: 'Type',
            render: (r) => (
              <Badge status={r.is_static ? undefined : 'info'} accent={r.is_static}>
                {r.is_static ? 'static' : 'dynamic'}
              </Badge>
            ),
          },
          { key: 'vlan', label: 'VLAN', sortable: true, render: (r) => <span className="cell-mono">{r.vlan}</span> },
          {
            key: 'interface', label: 'Interface',
            render: (r) => <span className="cell-mono">{shortName(r.interface)}</span>,
          },
          {
            key: 'actions', label: '', width: 80,
            render: (r) => (
              <Button variant="link-neutral" sm icon="trash"
                aria-label={`Remove binding ${r.mac}`}
                onClick={() => (r.is_static ? removeStatic(r) : clear(r.mac))} />
            ),
          },
        ]}
        rows={data.bindings}
        placeholder="No bindings."
      />
      <BindingModal open={modal} interfaces={interfaces}
        onClose={() => setModal(false)}
        onSaved={(result) => { setModal(false); onSaved(result); }} />
    </>
  );
}

function StatisticsTab({ data, refresh }) {
  return (
    <>
      <h3 style={{ margin: '0 0 12px' }}>DHCP Snooping</h3>
      <Datagrid
        rowKey={(r) => r.vlan}
        onRefresh={refresh}
        columns={[
          { key: 'vlan', label: 'VLAN', sortable: true, render: (r) => <span className="cell-mono">{r.vlan}</span> },
          { key: 'packets', label: 'Packets', render: (r) => <span className="cell-mono">{r.packets}</span> },
          {
            key: 'dropped', label: 'Dropped',
            render: (r) => (
              <span className={r.dropped > 0 ? 'cell-mono' : 'cell-mono dim'}>{r.dropped}</span>
            ),
          },
        ]}
        rows={data.dhcp.stats}
        placeholder="No DHCP snooping statistics."
        footerText={`Untrusted server drops: ${data.dhcp.untrusted_server_drops}`}
      />
      <h3 style={{ margin: '24px 0 12px' }}>ARP Inspection</h3>
      <Datagrid
        rowKey={(r) => r.vlan}
        onRefresh={refresh}
        columns={[
          { key: 'vlan', label: 'VLAN', sortable: true, render: (r) => <span className="cell-mono">{r.vlan}</span> },
          { key: 'forwarded', label: 'Forwarded', render: (r) => <span className="cell-mono">{r.forwarded}</span> },
          {
            key: 'dropped', label: 'Dropped',
            render: (r) => (
              <span className={r.dropped > 0 ? 'cell-mono' : 'cell-mono dim'}>{r.dropped}</span>
            ),
          },
          { key: 'bad_binding', label: 'Bad Binding', render: (r) => <span className="cell-mono">{r.bad_binding}</span> },
          { key: 'bad_src_mac', label: 'Bad Src MAC', render: (r) => <span className="cell-mono">{r.bad_src_mac}</span> },
        ]}
        rows={data.arp.stats}
        placeholder="No ARP inspection statistics."
      />
    </>
  );
}

export default function SnoopingSecPage() {
  const [tab, setTab] = useState('config');
  const [data, setData] = useState(null);
  const [interfaces, setInterfaces] = useState([]);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);

  const refresh = useCallback(() => {
    api('/api/snooping-sec')
      .then(setData)
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);
  useEffect(() => {
    api('/api/interfaces')
      .then((r) => setInterfaces(
        r.interfaces
          .map((i) => i.name)
          .filter((n) => n.startsWith('Ethernet') || n.startsWith('Port-Channel'))
          .sort(compareNames)
      ))
      .catch(() => {});
  }, []);

  const onSaved = (result) => {
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    setError(null);
    refresh();
  };

  const onError = (message) => setError(message);

  return (
    <Shell>
      <div className="page-header">
        <h2>DHCP Snooping &amp; ARP Inspection</h2>
        <span style={{ display: 'inline-flex', gap: 4 }}>
          {[['config', 'Config'], ['bindings', 'Bindings'], ['stats', 'Statistics']].map(([id, label]) => (
            <Button key={id} variant={tab === id ? 'primary' : 'outline'} sm
              onClick={() => setTab(id)}>
              {label}
            </Button>
          ))}
        </span>
      </div>
      {error && <Alert status="danger" closable onClose={() => setError(null)}
        style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!data && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {data && !data.live && tab !== 'config' && (
        <Alert status="warning" style={{ marginBottom: 16 }}>
          Live snooping state is unavailable; showing configured statics only.
        </Alert>
      )}
      {data && tab === 'config' && (
        <ConfigTab data={data} interfaces={interfaces} onSaved={onSaved} onError={onError} />
      )}
      {data && tab === 'bindings' && (
        <BindingsTab data={data} interfaces={interfaces} onSaved={onSaved} onError={onError}
          setApplied={setApplied} refresh={refresh} />
      )}
      {data && tab === 'stats' && <StatisticsTab data={data} refresh={refresh} />}
    </Shell>
  );
}
