'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Badge, Card, CardBlock } from '@/components/ds/misc';
import { Button } from '@/components/ds/Button';
import { FormField, Input, Textarea, SearchSelect } from '@/components/ds/forms';

const MAX_NAME_SERVERS = 3;

// The same RFC-1123 rule the CLI checks at the prompt; mgmtd
// re-validates on commit.
const validLabel = (label) =>
  !!label && label.length <= 63 && /^[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?$/.test(label);

const validDomain = (name) => {
  const trimmed = name.endsWith('.') ? name.slice(0, -1) : name;
  return !!trimmed && trimmed.length <= 253 && trimmed.split('.').every(validLabel);
};

const validAddress = (text) =>
  /^[0-9]{1,3}(\.[0-9]{1,3}){3}$/.test(text) || /^[0-9A-Fa-f:]+$/.test(text);

// The config leaf is one line; `\n` inside it becomes a real break when
// the banner is rendered, so the textarea round-trips through it.
const bannerToField = (text) => (text || '').replace(/\\n/g, '\n');
const bannerToLeaf = (text) => text.replace(/\r?\n/g, '\\n');

/// A resolver list editor: up to three rows, add/remove inline.
function NameServers({ values, onChange, disabled }) {
  const setAt = (index, value) => onChange(values.map((v, i) => (i === index ? value : v)));
  return (
    <div style={{ display: 'grid', gap: 8 }}>
      {values.map((value, index) => (
        <div key={index} style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          <Input
            className="mono"
            value={value}
            disabled={disabled}
            placeholder="10.42.0.5"
            aria-label={`Name server ${index + 1}`}
            onChange={(e) => setAt(index, e.target.value)}
          />
          <Button
            variant="link-neutral"
            sm
            icon="trash"
            disabled={disabled}
            aria-label={`Remove name server ${index + 1}`}
            onClick={() => onChange(values.filter((_, i) => i !== index))}
          />
        </div>
      ))}
      {values.length < MAX_NAME_SERVERS && (
        <div>
          <Button
            variant="link"
            sm
            icon="plus"
            disabled={disabled}
            onClick={() => onChange([...values, ''])}
          >
            Add Resolver
          </Button>
        </div>
      )}
      {values.length === 0 && (
        <span className="dim">No resolvers configured; systemd-resolved keeps its defaults.</span>
      )}
    </div>
  );
}

export default function SystemGeneralPage() {
  const [state, setState] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [busy, setBusy] = useState(false);

  // The editable copy; reset from the server on every load and commit.
  const [form, setForm] = useState(null);

  const refresh = useCallback(() => {
    api('/api/system/identity')
      .then((s) => {
        setState(s);
        setForm({
          hostname: s.hostname || '',
          timezone: s.timezone || '',
          domain_name: s.domain_name || '',
          name_servers: s.name_servers || [],
          banner_login: bannerToField(s.banner_login),
        });
      })
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  const field = (key) => (value) => setForm((f) => ({ ...f, [key]: value }));

  const problems = [];
  if (form) {
    if (form.hostname && !validLabel(form.hostname)) problems.push('Hostname is not a valid name.');
    if (form.domain_name && !validDomain(form.domain_name))
      problems.push('Domain name is not valid.');
    if (form.name_servers.some((s) => s.trim() && !validAddress(s.trim())))
      problems.push('One of the resolvers is not an IP address.');
  }

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/system/identity/edit', {
        method: 'POST',
        body: JSON.stringify({
          hostname: form.hostname.trim(),
          timezone: form.timezone,
          domain_name: form.domain_name.trim(),
          name_servers: form.name_servers.map((s) => s.trim()).filter(Boolean),
          banner_login: bannerToLeaf(form.banner_login).trim(),
        }),
      });
      setApplied(result.applied.length ? result.applied : ['No changes needed.']);
      refresh();
    } catch (err) {
      setError(err.message);
    } finally {
      setBusy(false);
    }
  };

  const zones = (state && state.timezones) || [];
  const zoneOptions = zones.map((z) => ({ value: z, label: z }));
  // A configured zone the installed database no longer carries must
  // still be selectable, or saving anything else would silently drop it.
  if (form && form.timezone && !zones.includes(form.timezone)) {
    zoneOptions.unshift({ value: form.timezone, label: `${form.timezone} (not installed)` });
  }

  return (
    <Shell>
      <div className="page-header">
        <h2>General</h2>
        <Button
          variant="primary"
          sm
          loading={busy}
          disabled={busy || !form || problems.length > 0}
          onClick={submit}
        >
          Commit
        </Button>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {problems.length > 0 && (
        <Alert status="warning" items={problems} style={{ marginBottom: 16 }} />
      )}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)} items={applied}
          style={{ marginBottom: 16 }} />
      )}
      {!state && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {state && form && (
        <>
          <Card
            header={
              <span style={{ display: 'inline-flex', alignItems: 'center', gap: 12 }}>
                Running
                {state.os_hostname && state.hostname && state.os_hostname !== state.hostname && (
                  <Badge status="warning">Hostname differs from config</Badge>
                )}
              </span>
            }
            style={{ marginBottom: 16 }}
          >
            <div style={{ display: 'grid', gridTemplateColumns: 'repeat(2, 1fr)', gap: 12 }}>
              <CardBlock title="Hostname" text={state.os_hostname || '—'} />
              <CardBlock title="Time Zone" text={state.os_timezone || '—'} />
            </div>
          </Card>

          <Card header="Identity" style={{ marginBottom: 16 }}>
            <div className="clr-form-compact" style={{ padding: 16 }}>
              <FormField
                label="Hostname"
                htmlFor="sys-hostname"
                helper="Letters, digits and hyphens; max 63. Empty means the default, hemlock."
                error={form.hostname && !validLabel(form.hostname) ? 'Not a valid name' : undefined}
              >
                <Input id="sys-hostname" className="mono" value={form.hostname}
                  onChange={(e) => field('hostname')(e.target.value)} />
              </FormField>
              <FormField label="Time Zone" htmlFor="sys-timezone"
                helper="From the installed tzdata; empty means UTC.">
                <SearchSelect
                  options={[{ value: '', label: 'UTC (default)' }, ...zoneOptions]}
                  value={form.timezone}
                  onChange={field('timezone')}
                  placeholder="UTC (default)"
                />
              </FormField>
              <FormField
                label="Domain Name"
                htmlFor="sys-domain"
                helper="Resolver search domain, and the FQDN in /etc/hosts."
                error={
                  form.domain_name && !validDomain(form.domain_name) ? 'Not a valid domain' : undefined
                }
              >
                <Input id="sys-domain" className="mono" value={form.domain_name}
                  onChange={(e) => field('domain_name')(e.target.value)} />
              </FormField>
              <FormField label="Name Servers" helper={`Up to ${MAX_NAME_SERVERS}, tried in order.`}>
                <NameServers values={form.name_servers} onChange={field('name_servers')}
                  disabled={busy} />
              </FormField>
            </div>
          </Card>

          <Card header="Login Banner">
            <div className="clr-form-compact" style={{ padding: 16 }}>
              <FormField
                label="Text"
                htmlFor="sys-banner"
                helper="Shown before authentication on ssh and the console. Empty removes it."
              >
                <Textarea id="sys-banner" rows={4} value={form.banner_login}
                  style={{ width: '100%', maxWidth: 'none' }}
                  onChange={(e) => field('banner_login')(e.target.value)} />
              </FormField>
            </div>
          </Card>
        </>
      )}
    </Shell>
  );
}
