'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, shortName, compareNames } from '@/lib/api';
import { Alert, Badge, Card, CardBlock, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Select, SearchSelect, Checkbox } from '@/components/ds/forms';

const FAMILIES = ['ipv4', 'ipv6', 'mac'];

const emptyRule = (number) => ({
  number: String(number),
  action: 'permit',
  protocol: '',
  source: '',
  destination: '',
  source_port: '',
  destination_port: '',
  dscp: '',
  log: false,
  police_rate: '',
  police_burst: '',
  source_mac: '',
  destination_mac: '',
  ethertype: '',
});

/// State-JSON rule → editor form values.
const ruleToForm = (r) => ({
  number: String(r.number),
  action: r.action,
  protocol: r.protocol || '',
  source: r.source || '',
  destination: r.destination || '',
  source_port: r.source_port || '',
  destination_port: r.destination_port || '',
  dscp: r.dscp != null ? String(r.dscp) : '',
  log: !!r.log,
  police_rate: r.police ? r.police.rate : '',
  police_burst: r.police && r.police.burst ? r.police.burst : '',
  source_mac: r.source_mac || '',
  destination_mac: r.destination_mac || '',
  ethertype: r.ethertype || '',
});

/// A one-line human summary of a rule's match fields for the detail table.
function matchSummary(rule, family) {
  const parts = [];
  if (family === 'mac') {
    if (rule.source_mac) parts.push(`src ${rule.source_mac}`);
    if (rule.destination_mac) parts.push(`dst ${rule.destination_mac}`);
    if (rule.ethertype) parts.push(`ethertype ${rule.ethertype}`);
  } else {
    if (rule.protocol) parts.push(rule.protocol);
    parts.push(rule.source || 'any');
    if (rule.source_port) parts.push(`sport ${rule.source_port}`);
    parts.push('→');
    parts.push(rule.destination || 'any');
    if (rule.destination_port) parts.push(`dport ${rule.destination_port}`);
    if (rule.dscp != null && rule.dscp !== '') parts.push(`dscp ${rule.dscp}`);
  }
  return parts.length ? parts.join(' ') : 'any';
}

function RuleFields({ rule, family, onChange }) {
  const set = (field) => (e) => onChange({ ...rule, [field]: e.target.value });
  const mono = { maxWidth: 'none' };
  return (
    <div style={{ display: 'grid', gridTemplateColumns: 'repeat(4, minmax(0, 1fr))', gap: '4px 12px' }}>
      <FormField label="Rule #" required>
        <Input className="mono" value={rule.number} onChange={set('number')} style={mono} />
      </FormField>
      <FormField label="Action" required>
        <Select options={['permit', 'deny']} value={rule.action}
          onChange={(e) => onChange({ ...rule, action: e.target.value })} />
      </FormField>
      {family !== 'mac' ? (
        <>
          <FormField label="Protocol" helper="tcp|udp|icmp|0-255">
            <Input className="mono" value={rule.protocol} onChange={set('protocol')} style={mono} />
          </FormField>
          <FormField label="DSCP" helper="0-63">
            <Input className="mono" value={rule.dscp} onChange={set('dscp')} style={mono} />
          </FormField>
          <FormField label="Source" helper="Prefix; empty = any">
            <Input className="mono" value={rule.source} onChange={set('source')} style={mono} />
          </FormField>
          <FormField label="Source Port" helper="443 or 67-68">
            <Input className="mono" value={rule.source_port} onChange={set('source_port')} style={mono} />
          </FormField>
          <FormField label="Destination" helper="Prefix; empty = any">
            <Input className="mono" value={rule.destination} onChange={set('destination')} style={mono} />
          </FormField>
          <FormField label="Destination Port" helper="443 or 67-68">
            <Input className="mono" value={rule.destination_port} onChange={set('destination_port')} style={mono} />
          </FormField>
          <FormField label="Police Rate" helper="e.g. 10m or 2000pps">
            <Input className="mono" value={rule.police_rate} onChange={set('police_rate')} style={mono} />
          </FormField>
          <FormField label="Police Burst" helper="e.g. 256k or 64pkts">
            <Input className="mono" value={rule.police_burst} onChange={set('police_burst')} style={mono} />
          </FormField>
          <FormField label=" ">
            <Checkbox label="Log matches" checked={rule.log}
              onChange={(e) => onChange({ ...rule, log: e.target.checked })} />
          </FormField>
        </>
      ) : (
        <>
          <FormField label="Ethertype" helper="0xHHHH|ipv4|ipv6|arp">
            <Input className="mono" value={rule.ethertype} onChange={set('ethertype')} style={mono} />
          </FormField>
          <FormField label="Source MAC" helper="mac[/mask]; empty = any">
            <Input className="mono" value={rule.source_mac} onChange={set('source_mac')} style={mono} />
          </FormField>
          <FormField label="Destination MAC" helper="mac[/mask]; empty = any">
            <Input className="mono" value={rule.destination_mac} onChange={set('destination_mac')} style={mono} />
          </FormField>
        </>
      )}
    </div>
  );
}

/// Create ("New ACL") and edit share one dialog; the family and name are
/// fixed when editing. Rules are edited as a list and committed
/// wholesale with the ACL.
function AclModal({ open, acl, onClose, onSaved }) {
  const editing = !!acl;
  const [family, setFamily] = useState('ipv4');
  const [name, setName] = useState('');
  const [rules, setRules] = useState([]);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setFamily(editing ? acl.family : 'ipv4');
    setName(editing ? acl.name : '');
    setRules(editing ? acl.rules.map(ruleToForm) : [emptyRule(10)]);
    setError(null);
    setBusy(false);
  }, [open, editing, acl]);

  const addRule = () => {
    const last = rules.length ? parseInt(rules[rules.length - 1].number, 10) || 0 : 0;
    setRules([...rules, emptyRule(last + 10)]);
  };

  const submit = async () => {
    const payload = [];
    for (const rule of rules) {
      const number = parseInt(rule.number, 10);
      if (!Number.isInteger(number) || number < 1) {
        setError(`Rule number "${rule.number}" must be a positive integer.`);
        return;
      }
      payload.push({ ...rule, number });
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/acls/edit', {
        method: 'POST',
        body: JSON.stringify({ set: [{ family, name: name.trim(), rules: payload }] }),
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
      title={editing ? `Edit ACL ${acl.name}` : 'New ACL'}
      size="lg"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy || !name.trim()}>
            Commit
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <div style={{ display: 'flex', gap: 16 }}>
          <FormField label="Family" required>
            <Select options={FAMILIES} value={family} disabled={editing}
              onChange={(e) => setFamily(e.target.value)} />
          </FormField>
          <FormField label="Name" required helper="Letter first; letters/digits/_/-; max 32">
            <Input className="mono" value={name} disabled={editing} autoFocus={!editing}
              onChange={(e) => setName(e.target.value)} style={{ maxWidth: 220 }} />
          </FormField>
        </div>
        {rules.map((rule, index) => (
          <div key={index}
            style={{ border: '1px solid var(--clr-color-neutral-300, #444)', borderRadius: 4, padding: '8px 12px', marginBottom: 8 }}>
            <div style={{ display: 'flex', justifyContent: 'flex-end' }}>
              <Button variant="link-neutral" sm icon="trash" aria-label={`Remove rule ${rule.number}`}
                onClick={() => setRules(rules.filter((_, i) => i !== index))} />
            </div>
            <RuleFields rule={rule} family={family}
              onChange={(next) => setRules(rules.map((r, i) => (i === index ? next : r)))} />
          </div>
        ))}
        <Button variant="outline" sm icon="plus" onClick={addRule}>Add Rule</Button>
        <p className="dim" style={{ marginTop: 8 }}>
          Rules are committed as a unit; anything not permitted is dropped by the implicit deny.
        </p>
      </div>
    </Modal>
  );
}

function BindingModal({ open, acls, interfaces, onClose, onSaved }) {
  const [iface, setIface] = useState('');
  const [acl, setAcl] = useState('');
  const [direction, setDirection] = useState('in');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setIface('');
    setAcl('');
    setDirection('in');
    setError(null);
    setBusy(false);
  }, [open]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/acls/bindings/edit', {
        method: 'POST',
        body: JSON.stringify({ set: [{ interface: iface, acl, direction }] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title="Bind ACL" size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy}
            disabled={busy || !iface || !acl}>
            Commit
          </Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Interface" required>
          <SearchSelect options={interfaces.map((name) => ({ value: name, label: name }))}
            value={iface} onChange={setIface} placeholder="Select interface…" />
        </FormField>
        <FormField label="ACL" required>
          <SearchSelect options={acls.map((a) => ({ value: a.name, label: `${a.name} (${a.family})` }))}
            value={acl} onChange={setAcl} placeholder="Select ACL…" />
        </FormField>
        <FormField label="Direction" required helper="One binding per direction">
          <Select options={['in', 'out']} value={direction}
            onChange={(e) => setDirection(e.target.value)} />
        </FormField>
      </div>
    </Modal>
  );
}

function DeleteModal({ open, name, onClose, onSaved }) {
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (open) {
      setError(null);
      setBusy(false);
    }
  }, [open]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/acls/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: [{ name }] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title={`Delete ACL ${name}`} size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="danger" onClick={submit} loading={busy} disabled={busy}>Delete</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <p>
        ACL <span className="mono">{name}</span> and all of its rules will be removed from the
        configuration. Any port bindings must be removed first.
      </p>
    </Modal>
  );
}

export default function AclsPage() {
  const [data, setData] = useState(null);
  const [interfaces, setInterfaces] = useState([]);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api('/api/acls')
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
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const clearCounters = async (name) => {
    setError(null);
    try {
      const result = await api('/api/acls/clear', {
        method: 'POST',
        body: JSON.stringify(name ? { name } : {}),
      });
      setApplied([`Cleared ${result.cleared} counter${result.cleared === 1 ? '' : 's'}.`]);
      refresh();
    } catch (err) {
      setError(err.message);
    }
  };

  const unbind = async (iface, direction) => {
    setError(null);
    try {
      const result = await api('/api/acls/bindings/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: [{ interface: iface, direction }] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  const detail = (acl) => (
    <div style={{ padding: '4px 8px' }}>
      <table className="datagrid-table" style={{ fontSize: 12 }}>
        <thead>
          <tr className="datagrid-row">
            <th className="datagrid-column">#</th>
            <th className="datagrid-column">Action</th>
            <th className="datagrid-column">Match</th>
            <th className="datagrid-column">Police</th>
            <th className="datagrid-column">Log</th>
            <th className="datagrid-column">Matches</th>
          </tr>
        </thead>
        <tbody>
          {acl.rules.map((rule) => (
            <tr key={rule.number} className="datagrid-row">
              <td className="datagrid-cell cell-mono">{rule.number}</td>
              <td className="datagrid-cell">
                <Label status={rule.action === 'permit' ? 'success' : 'danger'}>{rule.action}</Label>
              </td>
              <td className="datagrid-cell cell-mono">{matchSummary(rule, acl.family)}</td>
              <td className="datagrid-cell cell-mono">
                {rule.police ? `${rule.police.rate} / ${rule.police.burst || '—'}` : '—'}
              </td>
              <td className="datagrid-cell">{rule.log ? <Label>Log</Label> : <span className="dim">—</span>}</td>
              <td className="datagrid-cell cell-mono">{rule.matches}</td>
            </tr>
          ))}
          <tr className="datagrid-row">
            <td className="datagrid-cell dim">—</td>
            <td className="datagrid-cell"><Label status="danger">deny</Label></td>
            <td className="datagrid-cell dim">implicit deny (any)</td>
            <td className="datagrid-cell dim">—</td>
            <td className="datagrid-cell dim">—</td>
            <td className="datagrid-cell cell-mono">{acl.implicit_deny_matches}</td>
          </tr>
        </tbody>
      </table>
      <div style={{ display: 'flex', alignItems: 'center', gap: 12, marginTop: 8, flexWrap: 'wrap' }}>
        <span className="dim">Bindings:</span>
        {acl.bindings.length === 0 && <span className="dim">none</span>}
        {acl.bindings.map((b) => (
          <span key={`${b.port}-${b.direction}`}
            style={{ display: 'inline-flex', alignItems: 'center', gap: 2 }}>
            <span className="cell-mono">{shortName(b.port)} ({b.direction})</span>
            <Button variant="link-neutral" sm icon="trash"
              aria-label={`Unbind ${acl.name} from ${b.port} ${b.direction}`}
              onClick={() => unbind(b.port, b.direction)} />
          </span>
        ))}
        <Button variant="outline" sm onClick={() => clearCounters(acl.name)}>Clear Counters</Button>
      </div>
    </div>
  );

  return (
    <Shell>
      <div className="page-header">
        <h2>Access Control Lists</h2>
      </div>
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
          {data.tcam.length > 0 && (
            <Card header="TCAM Utilization" style={{ marginBottom: 16 }}>
              <div style={{ display: 'flex', gap: 32 }}>
                {data.tcam.map((stage) => (
                  <CardBlock key={stage.stage}
                    title={stage.stage === 'ingress' ? 'Ingress' : 'Egress'}
                    text={`${stage.used} used / ${stage.available} available`} />
                ))}
              </div>
            </Card>
          )}
          <Datagrid
            expandable
            rowKey={(r) => r.name}
            onRefresh={refresh}
            actionBar={() => (
              <>
                <Button variant="primary" sm icon="plus" onClick={() => setModal({ kind: 'new' })}>
                  New ACL
                </Button>
                <Button variant="outline" sm icon="link" onClick={() => setModal({ kind: 'bind' })}>
                  Bind ACL
                </Button>
                <Button variant="outline" sm onClick={() => clearCounters(null)}>
                  Clear All Counters
                </Button>
              </>
            )}
            renderDetail={detail}
            columns={[
              {
                key: 'name', label: 'Name', sortable: true,
                render: (r) => <span className="cell-mono">{r.name}</span>,
              },
              {
                key: 'family', label: 'Family',
                render: (r) => <Badge accent>{r.family}</Badge>,
              },
              {
                key: 'rules', label: 'Rules',
                render: (r) => <span className="cell-mono">{r.rules.length}</span>,
              },
              {
                key: 'bindings', label: 'Bindings',
                render: (r) => r.bindings.length
                  ? (
                    <span className="cell-mono">
                      {r.bindings.map((b) => `${shortName(b.port)} (${b.direction})`).join(', ')}
                    </span>
                  )
                  : <span className="dim">—</span>,
              },
              {
                key: 'total_matches', label: 'Matches', sortable: true,
                render: (r) => <span className="cell-mono">{r.total_matches}</span>,
              },
              {
                key: 'actions', label: '', width: 80,
                render: (r) => (
                  <span style={{ display: 'inline-flex', gap: 2 }}>
                    <Button variant="link-neutral" sm icon="pencil" aria-label={`Edit ACL ${r.name}`}
                      onClick={() => setModal({ kind: 'edit', acl: r })} />
                    <Button variant="link-neutral" sm icon="trash" aria-label={`Delete ACL ${r.name}`}
                      onClick={() => setModal({ kind: 'delete', name: r.name })} />
                  </span>
                ),
              },
            ]}
            rows={data.acls}
            placeholder="No ACLs configured."
          />
        </>
      )}
      <AclModal
        open={!!modal && (modal.kind === 'new' || modal.kind === 'edit')}
        acl={modal && modal.kind === 'edit' ? modal.acl : null}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
      <BindingModal
        open={!!modal && modal.kind === 'bind'}
        acls={data ? data.acls : []}
        interfaces={interfaces}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
      <DeleteModal
        open={!!modal && modal.kind === 'delete'}
        name={modal && modal.kind === 'delete' ? modal.name : ''}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
    </Shell>
  );
}
