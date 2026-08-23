'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, shortName, compareNames } from '@/lib/api';
import { Alert, Badge, Card, CardBlock } from '@/components/ds/misc';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Select } from '@/components/ds/forms';

const DIRECTIONS = [
  { value: 'both', label: 'Both' },
  { value: 'rx', label: 'Rx' },
  { value: 'tx', label: 'Tx' },
];

/// Create and edit share one dialog; sources and destination replace
/// the session's current program.
function SessionModal({ open, session, sessions, interfaces, lags, onClose, onSaved }) {
  const editing = !!session;
  const [id, setId] = useState('1');
  const [destination, setDestination] = useState('');
  const [sources, setSources] = useState([]);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    const taken = (sessions || []).map((s) => s.session);
    const free = [1, 2, 3, 4].find((n) => !taken.includes(n));
    setId(editing ? String(session.session) : String(free || 1));
    setDestination(editing ? session.destination : '');
    setSources(editing
      ? session.sources.map((s) => ({ port: s.port, direction: s.direction }))
      : []);
    setError(null);
    setBusy(false);
  }, [open, editing, session, sessions]);

  const ports = (interfaces || [])
    .filter((i) => i.kind === 'ethernet')
    .map((i) => i.name)
    .sort(compareNames);
  const lagMembers = new Set(
    (lags || []).flatMap((lag) => lag.members.map((m) => m.port)),
  );
  const sourcePorts = new Set(
    (sessions || [])
      .filter((s) => !editing || s.session !== session.session)
      .flatMap((s) => s.sources.map((src) => src.port))
      .concat(sources.map((s) => s.port)),
  );
  const routed = new Set(
    (interfaces || []).filter((i) => i.addresses.length > 0).map((i) => i.name),
  );

  // Destination pre-validation, mirroring the commit-time rules.
  const destinationProblem = (name) => {
    if (!name) return null;
    if (sourcePorts.has(name)) return 'is a mirror source';
    if (lagMembers.has(name)) return 'is a LAG member';
    if (routed.has(name)) return 'carries an address';
    return null;
  };

  const addSource = () => {
    const free = ports.find(
      (p) => p !== destination && !sources.some((s) => s.port === p),
    );
    if (free) setSources([...sources, { port: free, direction: 'both' }]);
  };

  const submit = async () => {
    const problem = destinationProblem(destination);
    if (destination && problem) {
      setError(`${shortName(destination)} ${problem} — pick another destination.`);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/mirror/edit', {
        method: 'POST',
        body: JSON.stringify({
          set: [{
            session: parseInt(id, 10),
            destination,
            sources,
          }],
        }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title={editing ? `Edit Session ${session.session}` : 'New Mirror Session'}
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <Alert status="warning" sm style={{ marginBottom: 12 }}>
        The destination port leaves normal switching: it stops forwarding regular traffic and
        only carries mirrored frames.
      </Alert>
      <div className="clr-form-compact">
        {!editing && (
          <FormField label="Session" htmlFor="mirror-session" helper="1..4">
            <Select id="mirror-session" value={id} onChange={(e) => setId(e.target.value)}
              options={[1, 2, 3, 4].map((n) => ({
                value: String(n),
                label: `Session ${n}`,
                disabled: (sessions || []).some((s) => s.session === n),
              }))} />
          </FormField>
        )}
        <FormField label="Destination" required htmlFor="mirror-destination"
          error={destination ? destinationProblem(destination) : undefined}>
          <Select id="mirror-destination" value={destination}
            onChange={(e) => setDestination(e.target.value)}
            options={[
              { value: '', label: 'Select a port…' },
              ...ports.map((p) => ({
                value: p,
                label: shortName(p) + (destinationProblem(p) ? ` (${destinationProblem(p)})` : ''),
                disabled: !!destinationProblem(p),
              })),
            ]} />
        </FormField>
        <FormField label="Source Ports">
          <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
            {sources.map((source, i) => (
              <div key={i} style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
                <Select value={source.port} aria-label={`Source ${i + 1}`}
                  onChange={(e) => setSources(sources.map((s, n) => (n === i ? { ...s, port: e.target.value } : s)))}
                  options={ports
                    .filter((p) => p !== destination)
                    .map((p) => ({ value: p, label: shortName(p) }))} />
                <Select value={source.direction} aria-label={`Direction ${i + 1}`}
                  onChange={(e) => setSources(sources.map((s, n) => (n === i ? { ...s, direction: e.target.value } : s)))}
                  options={DIRECTIONS} />
                <Button variant="link-neutral" sm icon="trash" aria-label="Remove source"
                  onClick={() => setSources(sources.filter((_, n) => n !== i))} />
              </div>
            ))}
            <Button variant="outline" sm icon="plus" onClick={addSource}>Add Source</Button>
          </div>
        </FormField>
      </div>
    </Modal>
  );
}

export default function MirrorPage() {
  const [sessions, setSessions] = useState(null);
  const [interfaces, setInterfaces] = useState(null);
  const [lags, setLags] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api('/api/mirror').then((r) => setSessions(r.sessions)).catch((e) => setError(e.message));
    api('/api/interfaces').then((r) => setInterfaces(r.interfaces)).catch(() => {});
    api('/api/lags').then((r) => setLags(r.lags)).catch(() => {});
  }, []);
  useEffect(refresh, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const remove = async (session) => {
    try {
      const result = await api('/api/mirror/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: [session] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  const byDirection = (session, direction) =>
    session.sources.filter((s) => s.direction === direction).map((s) => shortName(s.port));

  return (
    <Shell>
      <div className="page-header">
        <h2>Mirroring</h2>
        <Button variant="primary" sm icon="plus" onClick={() => setModal({ kind: 'new' })}
          disabled={sessions && sessions.length >= 4}>
          New Session
        </Button>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!sessions && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {sessions && sessions.length === 0 && (
        <p className="clr-secondary">No mirror sessions configured.</p>
      )}
      {sessions && sessions.map((session) => (
        <Card key={session.session} style={{ marginBottom: 16 }}
          header={
            <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', width: '100%', gap: 16 }}>
              <span>Session {session.session}</span>
              <span style={{ display: 'inline-flex', gap: 2 }}>
                <Button variant="link-neutral" sm icon="pencil"
                  aria-label={`Edit session ${session.session}`}
                  onClick={() => setModal({ kind: 'edit', session })} />
                <Button variant="link-neutral" sm icon="trash"
                  aria-label={`Delete session ${session.session}`}
                  onClick={() => remove(session.session)} />
              </span>
            </div>
          }>
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(4, minmax(0,1fr))', gap: 16 }}>
            <CardBlock title="Destination">
              <span style={{ display: 'inline-flex', gap: 8, alignItems: 'center' }}>
                <span className="cell-mono">{shortName(session.destination) || '—'}</span>
                {session.destination && (
                  <Badge status={session.destination_up ? 'success' : 'danger'}>
                    {session.destination_up ? 'Active' : 'Down'}
                  </Badge>
                )}
              </span>
            </CardBlock>
            {['both', 'rx', 'tx'].map((direction) => {
              const list = byDirection(session, direction);
              return (
                <CardBlock key={direction}
                  title={`${direction === 'both' ? 'Both' : direction.toUpperCase()} sources`}
                  text={list.length ? list.join(', ') : '—'} />
              );
            })}
          </div>
        </Card>
      ))}
      <SessionModal open={!!modal} session={modal && modal.kind === 'edit' ? modal.session : null}
        sessions={sessions} interfaces={interfaces} lags={lags}
        onClose={() => setModal(null)} onSaved={onSaved} />
    </Shell>
  );
}
