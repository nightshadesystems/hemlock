'use client';
import { useState } from 'react';
import { useRouter } from 'next/navigation';
import { api } from '@/lib/api';
import { Button } from '@/components/ds/Button';
import { FormField, Input, Password } from '@/components/ds/forms';
import { Alert } from '@/components/ds/misc';

function Wordmark({ size = 17, color }) {
  return (
    <span
      style={{
        fontFamily: 'var(--ns-font-brand)',
        fontWeight: 700,
        letterSpacing: '-0.02em',
        fontSize: size,
        color: color || 'var(--ns-ink-11)',
        lineHeight: 1,
        whiteSpace: 'nowrap',
      }}
    >
      Hemlock
    </span>
  );
}

export default function LoginPage() {
  const router = useRouter();
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api('/api/login', {
        method: 'POST',
        body: JSON.stringify({ username, password }),
      });
      router.replace('/dashboard/');
    } catch (err) {
      setError(err.status === 401 ? 'Sign-in failed — check your username and password.' : err.message);
      setBusy(false);
    }
  };

  return (
    <div className="login-wrapper">
      <div className="login-brand">
        <img src="/brand/nightshade-symbol-on-dark.svg" alt="Nightshade Systems" />
        <div className="login-brand-title">
          The whole switch,
          <br />
          one console.
        </div>
        <div className="login-brand-sub">
          Hemlock NOS puts interfaces, VLANs, and routing in front of you. You commit the config;
          the switch keeps it on every boot.
        </div>
      </div>
      <form className="login" onSubmit={submit}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 4 }}>
          <img src="/brand/nightshade-symbol-primary.svg" alt="" style={{ height: 28 }} />
          <Wordmark size={22} color="var(--cds-alias-typography-color-450)" />
        </div>
        <div className="title">Console</div>
        <div className="subtitle">Sign in with a switch operator account</div>
        {error && (
          <Alert status="danger" className="error">
            {error}
          </Alert>
        )}
        <FormField label="Username" htmlFor="username">
          <Input
            id="username"
            autoComplete="username"
            autoFocus
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            style={{ maxWidth: 'none' }}
          />
        </FormField>
        <FormField label="Password" htmlFor="password">
          <Password
            id="password"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            style={{ maxWidth: 'none' }}
          />
        </FormField>
        <Button variant="primary" block type="submit" loading={busy} disabled={busy || !username}>
          Sign in
        </Button>
        <div className="signup">Locked out? Sign in on the console port and reset the account.</div>
      </form>
    </div>
  );
}
