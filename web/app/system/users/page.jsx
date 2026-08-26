'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Badge, Card, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Password, Select, Textarea } from '@/components/ds/forms';

const MONO = { fontFamily: 'var(--ns-font-mono)', letterSpacing: '0.06em' };
const MAX_SSH_KEYS = 8;

// The account-name rule the CLI checks at the prompt; mgmtd
// re-validates on commit.
const validName = (name) => /^[a-z_][a-z0-9_-]{0,31}$/.test(name);

const SSH_KEY_TYPES = [
  'ssh-ed25519',
  'ssh-rsa',
  'ecdsa-sha2-nistp256',
  'ecdsa-sha2-nistp384',
  'ecdsa-sha2-nistp521',
  'sk-ssh-ed25519@openssh.com',
  'sk-ecdsa-sha2-nistp256@openssh.com',
  'rsa-sha2-256',
  'rsa-sha2-512',
];

const validKey = (key) => {
  const [type, body] = key.trim().split(/\s+/);
  return SSH_KEY_TYPES.includes(type) && !!body && body.length >= 16 && /^[A-Za-z0-9+/=]+$/.test(body);
};

// A duration as HH:MM:SS, the shape `show system users` prints.
const clock = (secs) => {
  const pad = (n) => String(n).padStart(2, '0');
  return `${pad(Math.floor(secs / 3600))}:${pad(Math.floor((secs % 3600) / 60))}:${pad(secs % 60)}`;
};

const stamp = (rfc3339) => (rfc3339 ? rfc3339.replace('T', ' ').replace('Z', '') : '—');

/// Create and edit share one dialog. The password is write-only: the
/// stored hash never leaves the switch, so an empty field means "leave
/// it alone" on an existing account.
function UserModal({ open, editing, users, adminsWithPassword, onClose, onSaved }) {
  const [name, setName] = useState('');
  const [role, setRole] = useState('operator');
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const [keys, setKeys] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setName(editing ? editing.name : '');
    setRole(editing ? editing.role : 'operator');
    setPassword('');
    setConfirm('');
    setKeys(editing ? (editing.ssh_keys || []).join('\n') : '');
    setError(null);
    setBusy(false);
  }, [open, editing]);

  const keyList = keys.split('\n').map((k) => k.trim()).filter(Boolean);
  const firstUser = users.length === 0;
  // Client-side mirror of the lockout guard, so the console explains
  // the refusal before the commit has to.
  const lastUsableAdmin =
    !!editing &&
    editing.role === 'admin' &&
    editing.auth === 'password' &&
    adminsWithPassword <= 1;

  const problems = [];
  if (!validName(name)) problems.push('Name must be a-z, 0-9, _ or -, starting with a letter or _.');
  if (password && password.length < 8) problems.push('Password must be at least 8 characters.');
  if (password !== confirm) problems.push('Passwords do not match.');
  if (keyList.length > MAX_SSH_KEYS) problems.push(`At most ${MAX_SSH_KEYS} SSH keys.`);
  if (keyList.some((k) => !validKey(k)))
    problems.push('Each SSH key must read "<type> <base64> [comment]".');
  if (!editing && !password && keyList.length === 0)
    problems.push('A new user needs a password or at least one SSH key.');
  if (firstUser && role !== 'admin')
    problems.push('The first configured user must be an administrator.');
  if (lastUsableAdmin && role !== 'admin')
    problems.push('This is the only administrator with a password; it cannot be demoted.');

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const body = { name, role, ssh_keys: keyList };
      // Absent means "leave the stored hash alone"; the field is only
      // sent when the operator actually typed a new password.
      if (password) body.password = password;
      const result = await api('/api/system/users/edit', {
        method: 'POST',
        body: JSON.stringify(body),
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
      title={editing ? `User · ${editing.name}` : 'Add User'}
      size="lg"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy}
            disabled={busy || problems.length > 0}>
            Commit
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      {problems.length > 0 && (
        <Alert status="warning" sm items={problems} style={{ marginBottom: 12 }} />
      )}
      <div className="clr-form-compact">
        <FormField label="Name" required htmlFor="user-name"
          helper="Lowercase letters, digits, - and _; max 32">
          <Input id="user-name" className="mono" autoFocus={!editing} value={name}
            disabled={!!editing}
            onChange={(e) => setName(e.target.value)} style={{ maxWidth: 'none' }} />
        </FormField>
        <FormField label="Role" htmlFor="user-role"
          helper="Operators see the whole console read-only; administrators may change it.">
          <Select id="user-role" value={role} onChange={(e) => setRole(e.target.value)}
            options={[
              { value: 'operator', label: 'Operator (read-only)' },
              { value: 'admin', label: 'Administrator' },
            ]} />
        </FormField>
        <FormField label="Password" htmlFor="user-password"
          helper={editing
            ? 'Leave empty to keep the current password. At least 8 characters.'
            : 'At least 8 characters. Hashed before it reaches the configuration.'}>
          <Password id="user-password" autoComplete="new-password" value={password}
            onChange={(e) => setPassword(e.target.value)} style={{ maxWidth: 'none' }} />
        </FormField>
        <FormField label="Confirm Password" htmlFor="user-confirm"
          error={confirm && confirm !== password ? 'Passwords do not match' : undefined}>
          <Password id="user-confirm" autoComplete="new-password" value={confirm}
            onChange={(e) => setConfirm(e.target.value)} style={{ maxWidth: 'none' }} />
        </FormField>
        <FormField label="SSH Keys" htmlFor="user-keys"
          helper={`One authorized_keys line per row; up to ${MAX_SSH_KEYS}.`}>
          <Textarea id="user-keys" rows={5} className="mono" value={keys}
            style={{ width: '100%', maxWidth: 'none' }}
            onChange={(e) => setKeys(e.target.value)} />
        </FormField>
      </div>
    </Modal>
  );
}

export default function UsersPage() {
  const [state, setState] = useState(null);
  const [session, setSession] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api('/api/system/users').then(setState).catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);
  useEffect(() => {
    api('/api/session').then(setSession).catch(() => {});
  }, []);

  // Sessions move on their own; poll while the page is open.
  useEffect(() => {
    const id = setInterval(refresh, 10_000);
    return () => clearInterval(id);
  }, [refresh]);

  const isAdmin = !session || session.admin;
  const readOnly = session && !session.admin ? 'Operator role: this console is read-only.' : null;

  const users = (state && state.users) || [];
  const adminsWithPassword = (state && state.admins_with_password) || 0;

  const onSaved = (result) => {
    setModal(null);
    const lines = result.applied.length ? result.applied : ['No changes needed.'];
    setApplied([...lines, ...(result.warnings || [])]);
    refresh();
  };

  const removeUser = async (user) => {
    try {
      const result = await api('/api/system/users/edit', {
        method: 'POST',
        body: JSON.stringify({ name: user.name, remove: true }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  return (
    <Shell>
      <div className="page-header">
        <h2>Users</h2>
        <Button variant="primary" sm icon="plus" disabled={!isAdmin}
          title={isAdmin ? undefined : 'Operator role: this console is read-only.'}
          onClick={() => setModal({})}>
          Add User
        </Button>
      </div>
      {readOnly && <Alert status="info" sm style={{ marginBottom: 16 }}>{readOnly}</Alert>}
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)} items={applied}
          style={{ marginBottom: 16 }} />
      )}
      {!state && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {state && (
        <>
          <Card header="Configured Users" style={{ marginBottom: 16 }}>
            <Datagrid
              rowKey={(r) => r.name}
              columns={[
                {
                  key: 'name', label: 'Name', sortable: true,
                  render: (r) => <span className="cell-mono">{r.name}</span>,
                },
                {
                  key: 'role', label: 'Role', sortable: true,
                  render: (r) =>
                    r.role === 'admin'
                      ? <Label accent>Administrator</Label>
                      : <Label>Operator</Label>,
                },
                {
                  key: 'auth', label: 'Auth',
                  render: (r) =>
                    r.auth === 'none'
                      ? <Label status="warning" style={MONO}>NO CREDENTIALS</Label>
                      : <span>{r.auth === 'password' ? 'Password' : 'SSH key'}</span>,
                },
                {
                  key: 'ssh_keys', label: 'SSH Keys',
                  render: (r) => <span className="cell-mono">{(r.ssh_keys || []).length}</span>,
                },
                {
                  key: 'actions', label: '', width: 80,
                  render: (r) => (
                    <span style={{ display: 'inline-flex', gap: 2 }}>
                      <Button variant="link-neutral" sm icon="pencil" disabled={!isAdmin}
                        aria-label={`Edit ${r.name}`}
                        onClick={() => setModal({ editing: r })} />
                      <Button variant="link-neutral" sm icon="trash"
                        disabled={
                          !isAdmin ||
                          (r.role === 'admin' && r.auth === 'password' && adminsWithPassword <= 1)
                        }
                        title={
                          r.role === 'admin' && r.auth === 'password' && adminsWithPassword <= 1
                            ? 'The only administrator with a password cannot be removed.'
                            : undefined
                        }
                        aria-label={`Remove ${r.name}`}
                        onClick={() => removeUser(r)} />
                    </span>
                  ),
                },
              ]}
              rows={users}
              placeholder="No users are managed by the configuration; the OS accounts stand as they are."
            />
          </Card>

          <Card
            header={
              <span style={{ display: 'inline-flex', alignItems: 'center', gap: 12 }}>
                Active Sessions
                <Badge>{`idle timeout ${state.session_timeout} min`}</Badge>
              </span>
            }
          >
            <Datagrid
              rowKey={(r, i) => `${r.user}-${r.client}-${i}`}
              onRefresh={refresh}
              columns={[
                {
                  key: 'user', label: 'User', sortable: true,
                  render: (r) => <span className="cell-mono">{r.user}</span>,
                },
                {
                  key: 'from', label: 'From',
                  render: (r) => <span className="cell-mono">{r.from}</span>,
                },
                {
                  key: 'client', label: 'Client',
                  render: (r) => <Label>{r.client === 'web' ? 'Web' : 'CLI'}</Label>,
                },
                { key: 'role', label: 'Role' },
                {
                  key: 'idle_secs', label: 'Idle', sortable: true,
                  render: (r) => <span className="cell-mono">{clock(r.idle_secs)}</span>,
                },
                {
                  key: 'login_time', label: 'Login Time',
                  render: (r) => <span className="cell-mono">{stamp(r.login_time)}</span>,
                },
              ]}
              rows={state.sessions || []}
              placeholder="No active sessions."
            />
          </Card>
        </>
      )}
      <UserModal
        open={!!modal}
        editing={modal && modal.editing}
        users={users}
        adminsWithPassword={adminsWithPassword}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
    </Shell>
  );
}
