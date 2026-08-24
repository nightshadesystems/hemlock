'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input } from '@/components/ds/forms';

/// Override a class's rate/burst; empty fields revert that knob to the
/// compiled default.
function ClassModal({ open, cls, onClose, onSaved }) {
  const [rate, setRate] = useState('');
  const [burst, setBurst] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open || !cls) return;
    setRate(String(cls.rate));
    setBurst(String(cls.burst));
    setError(null);
    setBusy(false);
  }, [open, cls]);

  const submit = async () => {
    const set = { class: cls.class };
    if (rate.trim()) {
      const parsed = parseInt(rate, 10);
      if (!Number.isInteger(parsed) || parsed < 1) {
        setError('Rate must be a positive integer (pps).');
        return;
      }
      set.rate = parsed;
    }
    if (burst.trim()) {
      const parsed = parseInt(burst, 10);
      if (!Number.isInteger(parsed) || parsed < 1) {
        setError('Burst must be a positive integer (packets).');
        return;
      }
      set.burst = parsed;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/copp/edit', {
        method: 'POST',
        body: JSON.stringify({ set: [set] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open} title={cls ? `CoPP Class ${cls.class}` : ''} size="sm" onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Rate (pps)" htmlFor="copp-rate" helper="1..10000000; empty = default">
          <Input id="copp-rate" className="mono" value={rate} autoFocus
            onChange={(e) => setRate(e.target.value)} style={{ maxWidth: 160 }} />
        </FormField>
        <FormField label="Burst (packets)" htmlFor="copp-burst" helper="1..1000000; empty = default">
          <Input id="copp-burst" className="mono" value={burst}
            onChange={(e) => setBurst(e.target.value)} style={{ maxWidth: 160 }} />
        </FormField>
      </div>
    </Modal>
  );
}

export default function CoppPage() {
  const [classes, setClasses] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);

  const refresh = useCallback(() => {
    api('/api/copp')
      .then((r) => setClasses(r.classes))
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  const restoreDefaults = async (cls) => {
    setError(null);
    try {
      const result = await api('/api/copp/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: [cls] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  const clearCounters = async () => {
    setError(null);
    try {
      await api('/api/copp/clear', { method: 'POST', body: JSON.stringify({}) });
      setApplied(['Cleared CoPP counters.']);
      refresh();
    } catch (err) {
      setError(err.message);
    }
  };

  return (
    <Shell>
      <div className="page-header">
        <h2>Control-Plane Policing</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!classes && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {classes && (
        <Datagrid
          rowKey={(r) => r.class}
          onRefresh={refresh}
          actionBar={() => (
            <Button variant="outline" sm onClick={clearCounters}>Clear Counters</Button>
          )}
          columns={[
            {
              key: 'class', label: 'Class', sortable: true,
              render: (r) => (
                <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}>
                  <span className="cell-mono">{r.class}</span>
                  {r.overridden && <Label status="info">Custom</Label>}
                </span>
              ),
            },
            { key: 'rate', label: 'Rate (pps)', sortable: true, render: (r) => <span className="cell-mono">{r.rate}</span> },
            { key: 'burst', label: 'Burst (pkts)', render: (r) => <span className="cell-mono">{r.burst}</span> },
            { key: 'conforming', label: 'Conforming', render: (r) => <span className="cell-mono">{r.conforming}</span> },
            {
              key: 'dropped', label: 'Dropped',
              render: (r) => (
                <span className={r.dropped > 0 ? 'cell-mono' : 'cell-mono dim'}>{r.dropped}</span>
              ),
            },
            {
              key: 'actions', label: '', width: 80,
              render: (r) => (
                <span style={{ display: 'inline-flex', gap: 2 }}>
                  <Button variant="link-neutral" sm icon="pencil" aria-label={`Edit class ${r.class}`}
                    onClick={() => setModal({ kind: 'edit', cls: r })} />
                  <Button variant="link-neutral" sm icon="undo" aria-label={`Restore class ${r.class} defaults`}
                    disabled={!r.overridden}
                    onClick={() => restoreDefaults(r.class)} />
                </span>
              ),
            },
          ]}
          rows={classes}
          placeholder="CoPP state unavailable."
        />
      )}
      <ClassModal
        open={!!modal && modal.kind === 'edit'}
        cls={modal && modal.kind === 'edit' ? modal.cls : null}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
    </Shell>
  );
}
