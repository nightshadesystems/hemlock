'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Badge, Card, CardBlock } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Password } from '@/components/ds/forms';

const MIN_PASSWORD = 8;

// A letter, then letters/digits/_/- — the same rule the CLI checks at
// the prompt; mgmtd re-validates on commit.
const validName = (name) => /^[A-Za-z][A-Za-z0-9_-]{0,31}$/.test(name || '');

/// Location and contact, the two free-text system-group fields.
function SettingsModal({ open, state, onClose, onSaved }) {
  const [location, setLocation] = useState('');
  const [contact, setContact] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open || !state) return;
    setLocation(state.location || '');
    setContact(state.contact || '');
    setError(null);
    setBusy(false);
  }, [open, state]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/snmp/edit', {
        method: 'POST',
        body: JSON.stringify({ location, contact }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title="SNMP system group" size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Location" htmlFor="snmp-location" helper="sysLocation; empty clears it">
          <Input id="snmp-location" value={location} onChange={(e) => setLocation(e.target.value)} />
        </FormField>
        <FormField label="Contact" htmlFor="snmp-contact" helper="sysContact; empty clears it">
          <Input id="snmp-contact" value={contact} onChange={(e) => setContact(e.target.value)} />
        </FormField>
      </div>
    </Modal>
  );
}

/// Add/edit one v2c community. The whole list is sent on commit,
/// because order decides which entry answers first.
function CommunityModal({ open, communities, editing, onClose, onSaved }) {
  const [name, setName] = useState('');
  const [source, setSource] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setName(editing ? editing.name : '');
    setSource(editing ? editing.source || '' : '');
    setError(null);
    setBusy(false);
  }, [open, editing]);

  const submit = async () => {
    if (!validName(name)) {
      setError('A community name starts with a letter (letters, digits, _ or -; max 32).');
      return;
    }
    const entry = { name, source: source.trim() };
    const next = editing
      ? communities.map((c) => (c.name === editing.name ? entry : { name: c.name, source: c.source || '' }))
      : [...communities.map((c) => ({ name: c.name, source: c.source || '' })), entry];
    if (!editing && communities.some((c) => c.name === name)) {
      setError(`Community "${name}" already exists.`);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/snmp/edit', {
        method: 'POST',
        body: JSON.stringify({ communities: next }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title={editing ? `Community · ${editing.name}` : 'Add Community'} size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy || !name}>
            Commit
          </Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Community" required htmlFor="snmp-community" helper="Read-only v2c access">
          <Input id="snmp-community" className="mono" value={name} disabled={!!editing}
            onChange={(e) => setName(e.target.value)} />
        </FormField>
        <FormField label="Source" htmlFor="snmp-source"
          helper="Restrict queriers to a prefix, e.g. 10.42.0.0/16; empty answers anywhere">
          <Input id="snmp-source" className="mono" value={source}
            onChange={(e) => setSource(e.target.value)} />
        </FormField>
      </div>
    </Modal>
  );
}

/// Add/edit one v3 USM user. Passphrases are write-only: the API never
/// returns them, and leaving a field blank on an edit keeps the
/// configured one.
function UserModal({ open, editing, onClose, onSaved }) {
  const [name, setName] = useState('');
  const [auth, setAuth] = useState('');
  const [priv, setPriv] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setName(editing || '');
    setAuth('');
    setPriv('');
    setError(null);
    setBusy(false);
  }, [open, editing]);

  const submit = async () => {
    if (!validName(name)) {
      setError('A user name starts with a letter (letters, digits, _ or -; max 32).');
      return;
    }
    if (!editing && (auth.length < MIN_PASSWORD || priv.length < MIN_PASSWORD)) {
      setError(`Both passphrases are required, at least ${MIN_PASSWORD} characters.`);
      return;
    }
    for (const [label, value] of [['Auth', auth], ['Privacy', priv]]) {
      if (value && value.length < MIN_PASSWORD) {
        setError(`${label} passphrase must be at least ${MIN_PASSWORD} characters.`);
        return;
      }
    }
    const set = { name };
    if (auth) set.auth_password = auth;
    if (priv) set.priv_password = priv;
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/snmp/edit', {
        method: 'POST',
        body: JSON.stringify({ users_set: [set] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title={editing ? `USM User · ${editing}` : 'Add USM User'} size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy || !name}>
            Commit
          </Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="User" required htmlFor="snmp-user" helper="Read-only authPriv (SHA / AES)">
          <Input id="snmp-user" className="mono" value={name} disabled={!!editing}
            onChange={(e) => setName(e.target.value)} />
        </FormField>
        <FormField label="Auth Passphrase" required={!editing} htmlFor="snmp-auth"
          helper={editing ? 'Leave blank to keep the configured passphrase' : `SHA; at least ${MIN_PASSWORD} characters`}>
          <Password id="snmp-auth" value={auth} onChange={(e) => setAuth(e.target.value)} />
        </FormField>
        <FormField label="Privacy Passphrase" required={!editing} htmlFor="snmp-priv"
          helper={editing ? 'Leave blank to keep the configured passphrase' : `AES; at least ${MIN_PASSWORD} characters`}>
          <Password id="snmp-priv" value={priv} onChange={(e) => setPriv(e.target.value)} />
        </FormField>
      </div>
    </Modal>
  );
}

export default function SnmpPage() {
  const [state, setState] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api('/api/snmp')
      .then(setState)
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  // The request counters move while a poller is running.
  useEffect(() => {
    const id = setInterval(refresh, 10_000);
    return () => clearInterval(id);
  }, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const edit = async (body) => {
    try {
      onSaved(await api('/api/snmp/edit', { method: 'POST', body: JSON.stringify(body) }));
    } catch (err) {
      setError(err.message);
    }
  };

  const communities = state ? state.communities : [];

  return (
    <Shell>
      <div className="page-header">
        <h2>SNMP</h2>
        {state && state.enabled && (
          <>
            <Button variant="outline" sm icon="pencil" onClick={() => setModal({ kind: 'settings' })}>
              System Group
            </Button>
            <Button variant="outline" sm icon="times"
              onClick={() => edit({ enabled: false })}>
              Disable Agent
            </Button>
          </>
        )}
        {state && !state.enabled && (
          <Button variant="primary" sm icon="plus" onClick={() => edit({ location: '' })}>
            Enable Agent
          </Button>
        )}
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!state && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {state && !state.enabled && (
        <Alert status="info">
          SNMP is not configured. Enabling the agent binds it to the management interface,
          which must carry an address.
        </Alert>
      )}
      {state && state.enabled && (
        <>
          <Card
            header={
              <span style={{ display: 'inline-flex', alignItems: 'center', gap: 12 }}>
                Agent
                <Badge status="success">Enabled</Badge>
                {!state.agentx_connected && (
                  <Badge status="warning">AgentX subagent disconnected</Badge>
                )}
              </span>
            }
            style={{ marginBottom: 16 }}
          >
            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3, 1fr)', gap: 12 }}>
              <CardBlock title="Listening On"
                text={`${state.listen_interface || '—'} ${state.listen_address ? `(${state.listen_address}:161)` : ''}`} />
              <CardBlock title="Location" text={state.location || '—'} />
              <CardBlock title="Contact" text={state.contact || '—'} />
              <CardBlock title="Packets In / Out"
                text={`${state.packets_in} / ${state.packets_out}`} />
              <CardBlock title="Get / GetNext-Bulk"
                text={`${state.get_requests} / ${state.getnext_requests}`} />
              <CardBlock title="Errors" text={String(state.errors)} />
            </div>
          </Card>

          <h3 style={{ margin: '0 0 12px' }}>Communities</h3>
          <Datagrid
            rowKey={(r) => r.name}
            onRefresh={refresh}
            actionBar={() => (
              <Button variant="primary" sm icon="plus"
                onClick={() => setModal({ kind: 'community' })}>
                Add Community
              </Button>
            )}
            columns={[
              {
                key: 'name', label: 'Community',
                render: (r) => <span className="cell-mono">{r.name}</span>,
              },
              { key: 'access', label: 'Access', render: () => <Badge>read-only</Badge> },
              {
                key: 'source', label: 'Source',
                render: (r) => r.source
                  ? <span className="cell-mono">{r.source}</span>
                  : <span className="dim">anywhere</span>,
              },
              {
                key: 'actions', label: '', width: 80,
                render: (r) => (
                  <span style={{ display: 'inline-flex', gap: 2 }}>
                    <Button variant="link-neutral" sm icon="pencil"
                      aria-label={`Edit ${r.name}`}
                      onClick={() => setModal({ kind: 'community', editing: r })} />
                    <Button variant="link-neutral" sm icon="trash"
                      aria-label={`Remove ${r.name}`}
                      onClick={() => edit({
                        communities: communities
                          .filter((c) => c.name !== r.name)
                          .map((c) => ({ name: c.name, source: c.source || '' })),
                      })} />
                  </span>
                ),
              },
            ]}
            rows={communities}
            placeholder="No v2c communities; only v3 users can query."
          />

          <h3 style={{ margin: '24px 0 12px' }}>USM Users (v3)</h3>
          <Datagrid
            rowKey={(r) => r}
            onRefresh={refresh}
            actionBar={() => (
              <Button variant="primary" sm icon="plus" onClick={() => setModal({ kind: 'user' })}>
                Add User
              </Button>
            )}
            columns={[
              { key: 'name', label: 'User', render: (r) => <span className="cell-mono">{r}</span> },
              { key: 'level', label: 'Security', render: () => <Badge>authPriv</Badge> },
              { key: 'auth', label: 'Auth', render: () => <span className="cell-mono">SHA</span> },
              { key: 'priv', label: 'Privacy', render: () => <span className="cell-mono">AES</span> },
              { key: 'access', label: 'Access', render: () => <Badge>read-only</Badge> },
              {
                key: 'actions', label: '', width: 80,
                render: (r) => (
                  <span style={{ display: 'inline-flex', gap: 2 }}>
                    <Button variant="link-neutral" sm icon="pencil"
                      aria-label={`Edit ${r}`}
                      onClick={() => setModal({ kind: 'user', editing: r })} />
                    <Button variant="link-neutral" sm icon="trash"
                      aria-label={`Remove ${r}`}
                      onClick={() => edit({ users_delete: [r] })} />
                  </span>
                ),
              },
            ]}
            rows={state.users}
            placeholder="No v3 users; only communities can query."
          />
        </>
      )}
      <SettingsModal open={!!modal && modal.kind === 'settings'} state={state}
        onClose={() => setModal(null)} onSaved={onSaved} />
      <CommunityModal open={!!modal && modal.kind === 'community'} communities={communities}
        editing={modal && modal.kind === 'community' ? modal.editing : null}
        onClose={() => setModal(null)} onSaved={onSaved} />
      <UserModal open={!!modal && modal.kind === 'user'}
        editing={modal && modal.kind === 'user' ? modal.editing : null}
        onClose={() => setModal(null)} onSaved={onSaved} />
    </Shell>
  );
}
