'use client';
import { useCallback, useEffect, useRef, useState } from 'react';
import Shell from '@/components/Shell';
import { api, shortName, compareNames, formatSpeed } from '@/lib/api';
import { Alert, Badge } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Checkbox } from '@/components/ds/forms';

const KINDS = ['broadcast', 'multicast', 'unknown-unicast'];

/// One dialog covers single-port edit and multi-select bulk apply.
function StormModal({ open, targets, interfaces, onClose, onSaved }) {
  const [levels, setLevels] = useState({ broadcast: '', multicast: '', 'unknown-unicast': '' });
  const [clears, setClears] = useState({});
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setLevels({ broadcast: '', multicast: '', 'unknown-unicast': '' });
    setClears({});
    setError(null);
    setBusy(false);
  }, [open]);

  // Computed-rate preview: percent of each target's link speed.
  const speedOf = (name) => {
    const iface = (interfaces || []).find((i) => i.name === name);
    return iface ? Number(iface.speed_mbps) : 0;
  };
  const preview = (kind) => {
    const pct = parseFloat(levels[kind]);
    if (!Number.isFinite(pct) || pct <= 0) return null;
    return targets
      .map((name) => {
        const speed = speedOf(name);
        if (!speed) return `${shortName(name)}: ?`;
        return `${shortName(name)}: ${formatSpeed(Math.round((speed * pct) / 100))}`;
      })
      .join(' · ');
  };

  const submit = async () => {
    const set = [];
    for (const name of targets) {
      for (const kind of KINDS) {
        if (clears[kind]) {
          set.push({ name, kind, level: '' });
        } else if (levels[kind] !== '') {
          set.push({ name, kind, level: levels[kind] });
        }
      }
    }
    if (set.length === 0) {
      setError('Set a level (or tick a clear) for at least one traffic class.');
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/storm-control/edit', {
        method: 'POST',
        body: JSON.stringify({ set }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal open={open}
      title={targets.length === 1
        ? `Storm control · ${targets[0]}`
        : `Storm control · ${targets.length} interfaces`}
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy}>Commit</Button>
        </>
      }>
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      {targets.length > 1 && (
        <p className="clr-secondary" style={{ marginBottom: 12 }}>
          Applies to: <span className="mono">{targets.map(shortName).join(', ')}</span>
        </p>
      )}
      <div className="clr-form-compact">
        {KINDS.map((kind) => (
          <div key={kind}>
            <FormField label={kind} htmlFor={`storm-${kind}`}
              helper={preview(kind) || 'Percent of link speed, 0.00..100.00; empty leaves unchanged'}>
              <div style={{ display: 'flex', gap: 12, alignItems: 'center' }}>
                <Input id={`storm-${kind}`} className="mono" value={levels[kind]}
                  placeholder="e.g. 10.00" disabled={!!clears[kind]}
                  onChange={(e) => setLevels({ ...levels, [kind]: e.target.value })}
                  style={{ maxWidth: 140 }} />
                <Checkbox label="Clear" checked={!!clears[kind]}
                  onChange={(e) => setClears({ ...clears, [kind]: e.target.checked })} />
              </div>
            </FormField>
          </div>
        ))}
      </div>
    </Modal>
  );
}

export default function StormControlPage() {
  const [entries, setEntries] = useState(null);
  const [interfaces, setInterfaces] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null);
  const clearSel = useRef(null);

  const refresh = useCallback(() => {
    api('/api/storm-control').then((r) => setEntries(r.entries)).catch((e) => setError(e.message));
    api('/api/interfaces').then((r) => setInterfaces(r.interfaces)).catch(() => {});
  }, []);
  useEffect(refresh, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    if (clearSel.current) clearSel.current();
    refresh();
  };

  // Rows keyed (name, kind); ports with no levels are absent — the
  // bulk-apply flow adds them via the interface list.
  const rows = entries
    ? [...entries].sort((a, b) => compareNames(a.name, b.name) || a.kind.localeCompare(b.kind))
    : null;
  const portNames = interfaces
    ? interfaces
        .filter((i) => (i.kind === 'ethernet' || i.kind === 'port-channel') && i.addresses.length === 0)
        .map((i) => i.name)
        .sort(compareNames)
    : [];

  return (
    <Shell>
      <div className="page-header">
        <h2>Storm Control</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!rows && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {rows && (
        <Datagrid
          selectable
          rowKey={(r) => `${r.name}-${r.kind}`}
          actionBar={({ selected, clear }) => {
            clearSel.current = clear;
            const names = [...new Set([...selected].map((key) => key.replace(/-[a-z-]+$/, '')))];
            return (
              <>
                <Button variant="primary" sm icon="plus"
                  onClick={() => setModal({ targets: portNames })}
                  disabled={portNames.length === 0}>
                  Bulk apply…
                </Button>
                <Button variant="outline" sm disabled={names.length === 0}
                  onClick={() => setModal({ targets: names })}>
                  Edit selected{names.length > 0 ? ` (${names.length})` : ''}
                </Button>
              </>
            );
          }}
          columns={[
            { key: 'name', label: 'Port', sortable: true, render: (r) => <span className="cell-mono">{shortName(r.name)}</span> },
            { key: 'kind', label: 'Type', render: (r) => r.kind },
            { key: 'level', label: 'Level', render: (r) => <span className="cell-mono">{r.level}%</span> },
            {
              key: 'rate', label: 'Rate',
              render: (r) => <span className="cell-mono">{formatSpeed(Math.round(r.rate_kbps / 1000))}</span>,
            },
            {
              key: 'drops', label: 'Drops',
              render: (r) => r.drops > 0
                ? <Badge status="warning">{r.drops}</Badge>
                : <span className="cell-mono">0</span>,
            },
            {
              key: 'active', label: 'Status',
              render: (r) => (
                <Badge status={r.active ? 'success' : undefined}>
                  {r.active ? 'active' : 'inactive'}
                </Badge>
              ),
            },
            {
              key: 'actions', label: '', width: 60,
              render: (r) => (
                <Button variant="link-neutral" sm icon="pencil"
                  aria-label={`Edit ${r.name}`} onClick={() => setModal({ targets: [r.name] })} />
              ),
            },
          ]}
          rows={rows}
          placeholder="No storm-control levels configured. Use Bulk apply to set some."
        />
      )}
      <StormModal open={!!modal} targets={modal ? modal.targets : []}
        interfaces={interfaces} onClose={() => setModal(null)} onSaved={onSaved} />
    </Shell>
  );
}
