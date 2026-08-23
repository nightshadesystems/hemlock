'use client';
import { useCallback, useEffect, useRef, useState } from 'react';
import Shell from '@/components/Shell';
import { api } from '@/lib/api';
import { Alert, Label } from '@/components/ds/misc';
import { Datagrid } from '@/components/ds/Datagrid';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { Checkbox, FormField, Input, Textarea } from '@/components/ds/forms';

/// Create ("New Route") and edit share one dialog; the prefix is fixed
/// when editing. The next-hop set is edited wholesale (one per line).
function RouteModal({ open, route, onClose, onSaved }) {
  const editing = !!route;
  const [prefix, setPrefix] = useState('');
  const [hops, setHops] = useState('');
  const [drop, setDrop] = useState(false);
  const [distance, setDistance] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setPrefix(editing ? route.prefix : '');
    setHops(editing ? (route.next_hops || []).join('\n') : '');
    setDrop(editing ? !!route.drop : false);
    setDistance(editing && route.distance !== 1 ? String(route.distance) : '');
    setError(null);
    setBusy(false);
  }, [open, editing, route]);

  const submit = async () => {
    const next_hops = drop
      ? []
      : hops.split(/[\s,]+/).map((h) => h.trim()).filter(Boolean);
    if (!drop && next_hops.length === 0) {
      setError('At least one next hop (or drop) is required.');
      return;
    }
    const set = { prefix: prefix.trim(), next_hops, drop };
    if (distance.trim()) {
      const parsed = parseInt(distance, 10);
      if (!Number.isInteger(parsed) || parsed < 1 || parsed > 255) {
        setError('Distance must be 1..255.');
        return;
      }
      set.distance = parsed;
    }
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/routes/static/edit', {
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
    <Modal
      open={open}
      title={editing ? `Edit Route ${route.prefix}` : 'New Route'}
      size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy || !prefix.trim()}>
            Commit
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Prefix" required htmlFor="route-prefix"
          helper="IPv4 or IPv6, e.g. 10.99.0.0/16 or 2001:db8::/48">
          <Input id="route-prefix" className="mono" value={prefix} disabled={editing}
            autoFocus={!editing} onChange={(e) => setPrefix(e.target.value)}
            style={{ maxWidth: 240 }} />
        </FormField>
        <FormField htmlFor="route-drop">
          <Checkbox label="Drop (null route)" checked={drop}
            onChange={(e) => setDrop(e.target.checked)} />
        </FormField>
        {!drop && (
          <FormField label="Next hops" required htmlFor="route-hops"
            helper="One per line; two or more = ECMP">
            <Textarea id="route-hops" className="mono" rows={3} value={hops}
              autoFocus={editing} onChange={(e) => setHops(e.target.value)}
              style={{ maxWidth: 240 }} />
          </FormField>
        )}
        <FormField label="Distance" htmlFor="route-distance" helper="1..255; empty = 1">
          <Input id="route-distance" className="mono" value={distance}
            onChange={(e) => setDistance(e.target.value)} style={{ maxWidth: 120 }} />
        </FormField>
      </div>
    </Modal>
  );
}

function DeleteModal({ open, prefixes, onClose, onSaved }) {
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (open) {
      setError(null);
      setBusy(false);
    }
  }, [open]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/routes/static/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: prefixes.map((prefix) => ({ prefix })) }),
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
      title={prefixes.length === 1 ? `Delete Route ${prefixes[0]}` : `Delete ${prefixes.length} Routes`}
      size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="danger" onClick={submit} loading={busy} disabled={busy}>
            Delete
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <p>
        Route{prefixes.length === 1 ? '' : 's'}{' '}
        <span className="mono">{prefixes.join(', ')}</span> — every next hop of the prefix — will
        be removed from the configuration.
      </p>
    </Modal>
  );
}

export default function StaticRoutesPage() {
  const [routes, setRoutes] = useState(null);
  const [error, setError] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null); // {kind:'new'} | {kind:'edit', route} | {kind:'delete', prefixes}
  const clearSel = useRef(null);

  const refresh = useCallback(() => {
    api('/api/routes')
      .then((r) => setRoutes(r.static_routes))
      .catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

  const onSaved = (result) => {
    setModal(null);
    setApplied(result.applied.length ? result.applied : ['No changes needed.']);
    if (clearSel.current) clearSel.current();
    refresh();
  };

  const removeHop = async (prefix, hop) => {
    setError(null);
    try {
      const result = await api('/api/routes/static/edit', {
        method: 'POST',
        body: JSON.stringify({ delete: [{ prefix, next_hop: hop }] }),
      });
      onSaved(result);
    } catch (err) {
      setError(err.message);
    }
  };

  return (
    <Shell>
      <div className="page-header">
        <h2>Static Routes</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {applied && (
        <Alert status="success" closable onClose={() => setApplied(null)}
          items={applied} style={{ marginBottom: 16 }} />
      )}
      {!routes && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {routes && (
        <Datagrid
          selectable
          expandable
          rowKey={(r) => r.prefix}
          onRefresh={refresh}
          actionBar={({ selected, clear }) => {
            clearSel.current = clear;
            const prefixes = [...selected].sort();
            return (
              <>
                <Button variant="primary" sm icon="plus" onClick={() => setModal({ kind: 'new' })}>
                  New Route
                </Button>
                <Button variant="danger-outline" sm disabled={prefixes.length === 0}
                  onClick={() => setModal({ kind: 'delete', prefixes })}>
                  Delete Selected{prefixes.length > 0 ? ` (${prefixes.length})` : ''}
                </Button>
              </>
            );
          }}
          renderDetail={(r) => (
            <div style={{ padding: '4px 8px' }}>
              {r.drop ? (
                <span className="dim">Null route — traffic to this prefix is dropped.</span>
              ) : (
                (r.next_hops || []).map((hop) => (
                  <div key={hop} style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
                    <span className="cell-mono">via {hop}</span>
                    <Button variant="link-neutral" sm icon="trash"
                      aria-label={`Remove next hop ${hop}`}
                      disabled={(r.next_hops || []).length === 1}
                      onClick={() => removeHop(r.prefix, hop)} />
                  </div>
                ))
              )}
            </div>
          )}
          columns={[
            {
              key: 'prefix', label: 'Prefix', sortable: true,
              render: (r) => <span className="cell-mono">{r.prefix}</span>,
            },
            {
              key: 'next_hops', label: 'Next Hops',
              render: (r) =>
                r.drop ? (
                  <Label status="danger">Drop</Label>
                ) : (
                  <span style={{ display: 'inline-flex', alignItems: 'center', gap: 8 }}>
                    <span className="cell-mono">{(r.next_hops || []).join(', ')}</span>
                    {(r.next_hops || []).length > 1 && <Label status="info">ECMP</Label>}
                  </span>
                ),
            },
            {
              key: 'distance', label: 'Distance', sortable: true,
              render: (r) => <span className="cell-mono">{r.distance}</span>,
            },
            {
              key: 'actions', label: '', width: 80,
              render: (r) => (
                <span style={{ display: 'inline-flex', gap: 2 }}>
                  <Button variant="link-neutral" sm icon="pencil" aria-label={`Edit route ${r.prefix}`}
                    onClick={() => setModal({ kind: 'edit', route: r })} />
                  <Button variant="link-neutral" sm icon="trash" aria-label={`Delete route ${r.prefix}`}
                    onClick={() => setModal({ kind: 'delete', prefixes: [r.prefix] })} />
                </span>
              ),
            },
          ]}
          rows={routes}
          placeholder="No static routes configured."
        />
      )}
      <RouteModal
        open={!!modal && (modal.kind === 'new' || modal.kind === 'edit')}
        route={modal && modal.kind === 'edit' ? modal.route : null}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
      <DeleteModal
        open={!!modal && modal.kind === 'delete'}
        prefixes={modal && modal.kind === 'delete' ? modal.prefixes : []}
        onClose={() => setModal(null)}
        onSaved={onSaved}
      />
    </Shell>
  );
}
