'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Password, Checkbox } from '@/components/ds/forms';

const MONO = { fontFamily: 'var(--ns-font-mono)', letterSpacing: '0.06em' };

function AddUserModal({ open, onClose, onSaved }) {
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const [sudo, setSudo] = useState(true);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (open) {
      setUsername('');
      setPassword('');
      setConfirm('');
      setSudo(true);
      setError(null);
      setBusy(false);
    }
  }, [open]);

  const submit = async () => {
    if (password !== confirm) {
      setError('Passwords do not match.');
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await api('/api/users/add', {
        method: 'POST',
        body: JSON.stringify({ username, password, sudo }),
      });
      onSaved(username);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      title="Add user"
      size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy}
            disabled={busy || !username || !password}>
            Add user
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Username" required htmlFor="new-username"
          helper="Lowercase letters, digits, - and _">
          <Input id="new-username" autoFocus value={username}
            onChange={(e) => setUsername(e.target.value)} style={{ maxWidth: 'none' }} />
        </FormField>
        <FormField label="Password" required htmlFor="new-password" helper="At least 8 characters">
          <Password id="new-password" autoComplete="new-password" value={password}
            onChange={(e) => setPassword(e.target.value)} style={{ maxWidth: 'none' }} />
        </FormField>
        <FormField label="Confirm password" required htmlFor="new-confirm"
          error={confirm && confirm !== password ? 'Passwords do not match' : undefined}>
          <Password id="new-confirm" autoComplete="new-password" value={confirm}
            onChange={(e) => setConfirm(e.target.value)} style={{ maxWidth: 'none' }} />
        </FormField>
        <div className="clr-form-control">
          <Checkbox label="Administrator (full sudo, like the stock admin account)"
            checked={sudo} onChange={(e) => setSudo(e.target.checked)} />
        </div>
      </div>
    </Modal>
  );
}

export default function UsersPage() {
  const [users, setUsers] = useState(null);
  const [error, setError] = useState(null);
  const [notice, setNotice] = useState(null);
  const [adding, setAdding] = useState(false);

  const refresh = useCallback(() => {
    api('/api/users')
      .then((r) => setUsers(r.users))
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  return (
    <Shell>
      <div className="page-header">
        <h2>Users</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {notice && (
        <Alert status="success" closable onClose={() => setNotice(null)} style={{ marginBottom: 16 }}>
          {notice}
        </Alert>
      )}
      {!users && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {users && (
        <Datagrid
          rowKey={(r) => r.username}
          actionBar={() => (
            <Button variant="primary" sm icon="plus" onClick={() => setAdding(true)}>
              Add user
            </Button>
          )}
          columns={[
            { key: 'username', label: 'Username', sortable: true, render: (r) => <span className="cell-mono">{r.username}</span> },
            { key: 'uid', label: 'UID', render: (r) => <span className="cell-mono">{r.uid ?? '—'}</span> },
            {
              key: 'sudo',
              label: 'Role',
              render: (r) => (r.sudo ? <Label accent>Administrator</Label> : <Label>Operator</Label>),
            },
            {
              key: 'locked',
              label: 'Status',
              render: (r) =>
                r.locked ? (
                  <Label status="warning" style={MONO}>LOCKED</Label>
                ) : (
                  <Label status="success" style={MONO}>ENABLED</Label>
                ),
            },
            { key: 'shell', label: 'Shell', render: (r) => <span className="cell-mono">{r.shell || '—'}</span> },
          ]}
          rows={users}
          placeholder="No operator accounts found."
        />
      )}
      <AddUserModal
        open={adding}
        onClose={() => setAdding(false)}
        onSaved={(username) => {
          setAdding(false);
          setNotice(`User ${username} created.`);
          refresh();
        }}
      />
    </Shell>
  );
}
