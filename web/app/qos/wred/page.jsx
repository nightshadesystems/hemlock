'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, shortName } from '@/lib/api';
import { Alert, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { Checkbox, FormField, Input } from '@/components/ds/forms';

const NAME_RE = /^[A-Za-z][A-Za-z0-9_-]{0,31}$/;

/// Create or edit a profile. The threshold sliders are capped at the
/// platform's packet buffer, which is exactly where commit caps them.
function ProfileModal({ open, profile, bufferKb, ecnSupported, onClose, onSaved }) {
  const [name, setName] = useState('');
  const [min, setMin] = useState(64);
  const [max, setMax] = useState(256);
  const [probability, setProbability] = useState(10);
  const [ecn, setEcn] = useState(false);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  const cap = Math.min(4096, bufferKb > 0 ? bufferKb : 4096);

  useEffect(() => {
    if (!open) return;
    setName(profile ? profile.name : '');
    setMin(profile && profile.min_threshold ? profile.min_threshold : 64);
    setMax(profile && profile.max_threshold ? profile.max_threshold : 256);
    setProbability(profile ? profile.drop_probability : 10);
    setEcn(profile ? profile.ecn : false);
    setError(null);
    setBusy(false);
  }, [open, profile]);

  const submit = async () => {
    if (!NAME_RE.test(name)) {
      setError('Name: letter first, then letters/digits/_/-, max 32.');
      return;
    }
    if (min >= max) {
      setError('Min threshold must be below max threshold.');
      return;
    }
    if (max > cap) {
      setError(`Max threshold exceeds the platform's ${cap} KB packet buffer.`);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/qos/wred/edit', {
        method: 'POST',
        body: JSON.stringify({
          set: [{
            name,
            min_threshold: String(min),
            max_threshold: String(max),
            drop_probability: String(probability),
            ecn,
          }],
        }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  const slider = (label, value, setValue, unit) => (
    <FormField label={label} helper={`1..${cap} ${unit}`}>
      <div style={{ display: 'flex', gap: 12, alignItems: 'center' }}>
        <input type="range" min={1} max={cap} value={value} style={{ flex: 1 }}
          onChange={(e) => setValue(parseInt(e.target.value, 10))} />
        <Input className="mono" value={String(value)} style={{ maxWidth: 90 }}
          onChange={(e) => {
            const parsed = parseInt(e.target.value, 10);
            setValue(Number.isInteger(parsed) ? parsed : 0);
          }} />
      </div>
    </FormField>
  );

  return (
    <Modal open={open} size="md" onClose={onClose}
      title={profile ? `WRED Profile ${profile.name}` : 'New WRED Profile'}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Name" htmlFor="wred-name"
          helper="Letter first, then letters/digits/_/-, max 32">
          <Input id="wred-name" className="mono" value={name} disabled={!!profile} autoFocus
            onChange={(e) => setName(e.target.value)} style={{ maxWidth: 240 }} />
        </FormField>
        {slider('Min threshold', min, setMin, 'KB')}
        {slider('Max threshold', max, setMax, 'KB')}
        <FormField label="Drop probability" helper="Percent at max threshold (1..100)">
          <div style={{ display: 'flex', gap: 12, alignItems: 'center' }}>
            <input type="range" min={1} max={100} value={probability} style={{ flex: 1 }}
              onChange={(e) => setProbability(parseInt(e.target.value, 10))} />
            <Input className="mono" value={String(probability)} style={{ maxWidth: 90 }}
              onChange={(e) => {
                const parsed = parseInt(e.target.value, 10);
                setProbability(Number.isInteger(parsed) ? parsed : 0);
              }} />
          </div>
        </FormField>
        <FormField label="ECN"
          helper={ecnSupported
            ? 'Mark ECT traffic instead of dropping it'
            : 'ECN marking is not supported by this platform’s SAI'}>
          <Checkbox label="Mark instead of drop" checked={ecn} disabled={!ecnSupported}
            onChange={(e) => setEcn(e.target.checked)} />
        </FormField>
      </div>
    </Modal>
  );
}

export default function QosWredPage() {
  const [state, setState] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api('/api/qos/wred')
      .then(setState)
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const remove = async (profile) => {
    setError(null);
    // A bound profile cannot go: say which queues hold it rather than
    // sending a commit that mgmtd will refuse.
    if (profile.references.length > 0) {
      setError(
        `${profile.name} is still bound by ${profile.references
          .map((r) => `${shortName(r.port)} q${r.queue}`)
          .join(', ')} — clear those queues first.`,
      );
      return;
    }
    try {
      const result = await api('/api/qos/wred/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: [profile.name] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  return (
    <Shell>
      <div className="page-header">
        <h2>WRED Profiles</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {state && !state.wred_supported && (
        <Alert status="warning" style={{ marginBottom: 16 }}>
          WRED is not supported by this platform’s SAI — a queue referencing a profile fails commit.
        </Alert>
      )}
      {!state && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {state && (
        <Datagrid
          rowKey={(r) => r.name}
          onRefresh={refresh}
          placeholder="No WRED profiles defined."
          footerText={state.buffer_kb > 0
            ? `${state.profiles.length} profiles · thresholds cap at the platform's ${state.buffer_kb} KB packet buffer`
            : `${state.profiles.length} profiles`}
          actionBar={() => (
            <Button variant="primary" sm icon="plus" onClick={() => setModal({ profile: null })}>
              New Profile
            </Button>
          )}
          columns={[
            {
              key: 'name', label: 'Profile', sortable: true,
              render: (r) => (
                <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}>
                  <span className="cell-mono">{r.name}</span>
                  {r.ecn && <Label status="info">ECN</Label>}
                </span>
              ),
            },
            {
              key: 'min_threshold', label: 'Min (KB)', sortable: true,
              render: (r) => <span className="cell-mono">{r.min_threshold || '—'}</span>,
            },
            {
              key: 'max_threshold', label: 'Max (KB)',
              render: (r) => <span className="cell-mono">{r.max_threshold || '—'}</span>,
            },
            {
              key: 'drop_probability', label: 'Drop Prob',
              render: (r) => <span className="cell-mono">{r.drop_probability}%</span>,
            },
            {
              key: 'references', label: 'References',
              render: (r) => (r.references.length === 0
                ? <span className="cell-mono dim">—</span>
                : (
                  <span style={{ display: 'inline-flex', gap: 4, flexWrap: 'wrap' }}>
                    {r.references.map((ref) => (
                      <Label key={`${ref.port}-${ref.queue}`}>
                        {shortName(ref.port)} q{ref.queue}
                      </Label>
                    ))}
                  </span>
                )),
            },
            {
              key: 'actions', label: '', width: 80,
              render: (r) => (
                <span style={{ display: 'inline-flex', gap: 2 }}>
                  <Button variant="link-neutral" sm icon="pencil"
                    aria-label={`Edit profile ${r.name}`}
                    onClick={() => setModal({ profile: r })} />
                  <Button variant="link-neutral" sm icon="trash"
                    aria-label={`Delete profile ${r.name}`}
                    onClick={() => remove(r)} />
                </span>
              ),
            },
          ]}
          rows={state.profiles}
        />
      )}
      <ProfileModal
        open={!!modal}
        profile={modal ? modal.profile : null}
        bufferKb={state ? state.buffer_kb : 0}
        ecnSupported={state ? state.ecn_supported : false}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
    </Shell>
  );
}
