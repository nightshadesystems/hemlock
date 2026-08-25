'use client';
import { useCallback, useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Card, CardBlock } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Input } from '@/components/ds/forms';

// The key domain per table, mirroring the config grammar: the two
// classification maps key on DSCP/CoS, the rewrite maps on the traffic
// class.
const KEY_MAX = { 'dscp-to-tc': 63, 'cos-to-tc': 7, 'tc-to-dscp': 7, 'tc-to-cos': 7 };
const VALUE_MAX = { 'dscp-to-tc': 7, 'cos-to-tc': 7, 'tc-to-dscp': 63, 'tc-to-cos': 7 };

// "40-46,48" → [40,41,...,46,48]; throws on junk. Mirror of the CLI's
// list expansion so a bad entry never reaches mgmtd.
function parseValueList(text, max, what) {
  const out = new Set();
  const parts = text.split(',').map((p) => p.trim()).filter(Boolean);
  if (parts.length === 0) throw new Error(`enter a ${what} value`);
  for (const part of parts) {
    const m = /^(\d+)(?:-(\d+))?$/.exec(part);
    if (!m) throw new Error(`bad ${what} entry "${part}"`);
    const from = parseInt(m[1], 10);
    const to = m[2] ? parseInt(m[2], 10) : from;
    if (to > max || from > to || (m[2] && from === to)) {
      throw new Error(`bad ${what} range "${part}" (0..${max}, low < high)`);
    }
    for (let v = from; v <= to; v++) out.add(v);
  }
  return [...out].sort((a, b) => a - b);
}

/// One map table: the existing entries with per-row delete, plus an
/// add-row that accepts a value, a list, or a range.
function MapCard({ table, onSaved, onError, disabled, disabledNote }) {
  const [key, setKey] = useState('');
  const [value, setValue] = useState('');
  const [busy, setBusy] = useState(false);

  const keyMax = KEY_MAX[table.table];
  const valueMax = VALUE_MAX[table.table];

  const add = async () => {
    onError(null);
    let parsedValue;
    try {
      parseValueList(key, keyMax, table.key_label);
      parsedValue = parseInt(value, 10);
      if (!Number.isInteger(parsedValue) || parsedValue < 0 || parsedValue > valueMax) {
        throw new Error(`${table.value_label} must be 0..${valueMax}`);
      }
    } catch (err) {
      onError(err.message);
      return;
    }
    setBusy(true);
    try {
      const result = await api('/api/qos/maps/edit', {
        method: 'POST',
        body: JSON.stringify({ set: [{ table: table.table, key, value: parsedValue }] }),
      });
      setKey('');
      setValue('');
      onSaved(result);
    } catch (err) {
      onError(err.message);
    } finally {
      setBusy(false);
    }
  };

  const remove = async (entryKey) => {
    onError(null);
    try {
      const result = await api('/api/qos/maps/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: [{ table: table.table, key: String(entryKey) }] }),
      });
      onSaved(result);
    } catch (err) {
      onError(err.message);
    }
  };

  const clear = async () => {
    onError(null);
    try {
      const result = await api('/api/qos/maps/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: [{ table: table.table, key: '' }] }),
      });
      onSaved(result);
    } catch (err) {
      onError(err.message);
    }
  };

  return (
    <Card header={table.title}>
      <CardBlock>
        <Datagrid
          compact
          rowKey={(r) => r.key}
          placeholder={`No ${table.key_label} mappings.`}
          footerText={`Unmapped ${table.key_label} → ${table.default_note}`}
          actionBar={() => (
            <Button variant="outline" sm disabled={disabled || table.entries.length === 0}
              onClick={clear}>
              Clear table
            </Button>
          )}
          columns={[
            {
              key: 'key',
              label: table.key_label,
              sortable: true,
              render: (r) => <span className="cell-mono">{r.key}</span>,
            },
            {
              key: 'value',
              label: table.value_label,
              render: (r) => <span className="cell-mono">{r.value}</span>,
            },
            {
              key: 'actions',
              label: '',
              width: 48,
              render: (r) => (
                <Button variant="link-neutral" sm icon="trash" disabled={disabled}
                  aria-label={`Remove ${table.key_label} ${r.key}`}
                  onClick={() => remove(r.key)} />
              ),
            },
          ]}
          rows={table.entries}
        />
        <div style={{ display: 'flex', gap: 8, alignItems: 'center', marginTop: 12 }}>
          <Input className="mono" value={key} disabled={disabled}
            placeholder={`${table.key_label} (e.g. 46 or 40-46,48)`}
            style={{ maxWidth: 220 }}
            onChange={(e) => setKey(e.target.value)} />
          <span style={{ opacity: 0.6 }}>→</span>
          <Input className="mono" value={value} disabled={disabled}
            placeholder={`${table.value_label} 0-${valueMax}`}
            style={{ maxWidth: 120 }}
            onChange={(e) => setValue(e.target.value)} />
          <Button variant="primary" sm loading={busy}
            disabled={disabled || busy || !key.trim() || !value.trim()} onClick={add}>
            Add
          </Button>
        </div>
        {disabled && (
          <Alert status="warning" sm style={{ marginTop: 12 }}>{disabledNote}</Alert>
        )}
      </CardBlock>
    </Card>
  );
}

export default function QosMapsPage() {
  const [state, setState] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);

  const refresh = useCallback(() => {
    api('/api/qos/maps')
      .then(setState)
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  const onSaved = (result) => {
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    refresh();
  };

  // The rewrite maps need the egress qos-map binding; classification
  // needs the ingress one. A platform without either gets a read-only
  // card rather than an editor whose commit would fail.
  const gate = (table) =>
    table.table.startsWith('tc-to-')
      ? [!state.qos_map_egress, 'Egress rewrite maps are not supported by this platform’s SAI.']
      : [!state.qos_map_ingress, 'Classification maps are not supported by this platform’s SAI.'];

  return (
    <Shell>
      <div className="page-header">
        <h2>QoS Maps</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!state && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {state && (
        <div style={{ display: 'grid', gap: 16, gridTemplateColumns: 'repeat(auto-fit, minmax(360px, 1fr))' }}>
          {state.tables.map((table) => {
            const [disabled, note] = gate(table);
            return (
              <MapCard key={table.table} table={table} onSaved={onSaved} onError={setError}
                disabled={disabled} disabledNote={note} />
            );
          })}
        </div>
      )}
    </Shell>
  );
}
