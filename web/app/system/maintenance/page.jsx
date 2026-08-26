'use client';
import { useCallback, useEffect, useRef, useState } from 'react';
import Shell from '@/components/Shell';
import { api, uploadFile, downloadText } from '@/lib/api';
import { Alert, Card, CardBlock, Label } from '@/components/ds/misc';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Checkbox, Select } from '@/components/ds/forms';
import { Datagrid } from '@/components/ds/Datagrid';

const MONO = { fontFamily: 'var(--ns-font-mono)', letterSpacing: '0.06em' };

const formatBytes = (n) => {
  if (n == null) return '—';
  if (n >= 1024 * 1024 * 1024) return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GiB`;
  if (n >= 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MiB`;
  return `${Math.max(1, Math.round(n / 1024))} KiB`;
};

const formatWhen = (unixSecs) =>
  new Date(unixSecs * 1000).toLocaleString([], {
    weekday: 'short', hour: '2-digit', minute: '2-digit',
    month: 'short', day: 'numeric',
  });

// An RFC-3339 commit time as the CLI prints it; `-` when the ring
// entry predates recorded metadata.
const commitStamp = (rfc3339) =>
  rfc3339 ? rfc3339.replace('T', ' ').replace('Z', '') : '—';

const stamp = () => {
  const d = new Date();
  const p = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}-${p(d.getHours())}${p(d.getMinutes())}`;
};

// Full-page hold while the switch reboots: poll until the API answers
// again, then reload into the fresh session.
function RebootingOverlay({ at }) {
  useEffect(() => {
    const timer = setInterval(() => {
      fetch('/api/session', { credentials: 'same-origin' })
        .then((r) => { if (r.ok || r.status === 401) window.location.reload(); })
        .catch(() => {});
    }, 5000);
    return () => clearInterval(timer);
  }, []);
  return (
    <div className="page-loading" style={{ flexDirection: 'column', gap: 16 }}>
      <span className="spinner spinner-md"></span>
      <div>
        {at ? 'The switch is rebooting.' : 'The switch is going down for a reboot.'}
        {' '}This page reconnects automatically when it is back.
      </div>
    </div>
  );
}

/// Plain-text editor with a scroll-synced line-number gutter.
function NumberedEditor({ value, onChange, rows = 14 }) {
  const gutterRef = useRef(null);
  const lineCount = Math.max(1, value.split('\n').length);
  const sync = (e) => {
    if (gutterRef.current) gutterRef.current.scrollTop = e.target.scrollTop;
  };
  return (
    <div className="numbered-editor">
      <div ref={gutterRef} className="numbered-editor-gutter" aria-hidden="true">
        {Array.from({ length: lineCount }, (_, i) => (
          <div key={i}>{i + 1}</div>
        ))}
      </div>
      <textarea className="clr-textarea numbered-editor-text" rows={rows} value={value}
        spellCheck={false} wrap="off" onChange={onChange} onScroll={sync} />
    </div>
  );
}

function RestoreModal({ open, onClose, onRestored }) {
  const [filename, setFilename] = useState(null);
  const [text, setText] = useState('');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);
  const fileRef = useRef(null);

  useEffect(() => {
    if (open) {
      setFilename(null);
      setText('');
      setError(null);
      setBusy(false);
    }
  }, [open]);

  const pick = async (e) => {
    const file = e.target.files && e.target.files[0];
    e.target.value = '';
    if (!file) return;
    setFilename(file.name);
    setText(await file.text());
    setError(null);
  };

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await api('/api/config/restore', {
        method: 'POST',
        body: JSON.stringify({ text }),
      });
      onRestored(r.applied || []);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      title="Restore Configuration"
      size="lg"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="danger" onClick={submit} loading={busy} disabled={busy || !text.trim()}>
            Validate & Apply
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <Alert status="warning" sm style={{ marginBottom: 12 }}>
        This replaces the entire running configuration. The current config is kept in the
        rollback history, but a bad restore can take the switch off the network.
      </Alert>
      {text.trim() !== '' && !/\bhttps?\b/.test(text) && (
        <Alert status="danger" sm style={{ marginBottom: 12 }}>
          This configuration has no <span className="mono">system http/https</span> service —
          applying it shuts down the web console, including this session.
        </Alert>
      )}
      <input ref={fileRef} type="file" accept=".conf,.txt,text/plain" hidden onChange={pick} />
      <div className="clr-form-compact">
        <FormField label="Configuration File" htmlFor="restore-file">
          <div style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
            <Button sm icon="folder-open" onClick={() => fileRef.current && fileRef.current.click()}>
              Choose File…
            </Button>
            <span className="cell-mono dim">{filename || 'no file selected'}</span>
          </div>
        </FormField>
        <FormField label="Contents" helper="Review (and edit if needed) before applying.">
          <NumberedEditor rows={14} value={text} onChange={(e) => setText(e.target.value)} />
        </FormField>
      </div>
    </Modal>
  );
}

/// Reboot, optionally into ONIE rescue. Both need `yes` typed in full,
/// the same confirmation the CLI asks for — a reboot is not something
/// to trigger with one stray click.
function RebootNowModal({ open, onieRescue, onClose, onRebooting }) {
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);
  const [typed, setTyped] = useState('');

  useEffect(() => {
    if (open) { setError(null); setBusy(false); setTyped(''); }
  }, [open]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      await api('/api/reboot', {
        method: 'POST',
        body: JSON.stringify({ in_minutes: 0, onie_rescue: !!onieRescue }),
      });
      onRebooting();
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      title={onieRescue ? 'Reboot into ONIE Rescue' : 'Reboot Switch'}
      size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="danger" onClick={submit} loading={busy}
            disabled={busy || typed.trim() !== 'yes'}>
            {onieRescue ? 'Reboot into ONIE' : 'Reboot Now'}
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <p>
        {onieRescue
          ? 'Reboot into ONIE rescue mode? The switch comes up in ONIE, not Hemlock — '
            + 'it stops forwarding and this console will not be there. The arming applies '
            + 'to the next boot only.'
          : 'Reboot the switch immediately? All ports go down until the switch has booted '
            + 'and reconverged, and unsaved candidate changes are discarded.'}
      </p>
      <div className="clr-form-compact">
        <FormField label="Confirmation" required htmlFor="reboot-confirm"
          helper="Type `yes` to enable the button.">
          <Input id="reboot-confirm" autoFocus className="mono" value={typed}
            onChange={(e) => setTyped(e.target.value)} />
        </FormField>
      </div>
    </Modal>
  );
}

/// Rolling back is a commit like any other: the dialog names exactly
/// which entry it is about to make current.
function RollbackModal({ open, commit, onClose, onRolledBack }) {
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (open) { setError(null); setBusy(false); }
  }, [open]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const result = await api('/api/system/rollback', {
        method: 'POST',
        body: JSON.stringify({ index: commit.index }),
      });
      onRolledBack(result);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  if (!commit) return null;
  return (
    <Modal
      open={open}
      title={`Roll Back to Commit ${commit.index}`}
      size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="warning" onClick={submit} loading={busy} disabled={busy}>
            Roll Back
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <p>
        Load commit {commit.index} ({commitStamp(commit.time)}
        {commit.user ? `, ${commit.user}` : ''}
        {commit.client ? `, ${commit.client}` : ''}) and commit it? The configuration running
        now becomes commit 1, so this is itself reversible.
      </p>
    </Modal>
  );
}

function ScheduleRebootModal({ open, onClose, onScheduled }) {
  const [minutes, setMinutes] = useState('60');
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (open) { setMinutes('60'); setError(null); setBusy(false); }
  }, [open]);

  const parsed = parseInt(minutes, 10);
  const valid = Number.isFinite(parsed) && parsed >= 1 && parsed <= 7 * 24 * 60;

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      await api('/api/reboot', { method: 'POST', body: JSON.stringify({ in_minutes: parsed }) });
      onScheduled();
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      title="Schedule Reboot"
      size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={submit} loading={busy} disabled={busy || !valid}>
            Schedule
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <div className="clr-form-compact">
        <FormField label="Reboot in (minutes)" required htmlFor="reboot-minutes"
          helper={valid ? `Reboots around ${formatWhen(Date.now() / 1000 + parsed * 60)}` : '1 to 10080 minutes (one week)'}
          error={minutes && !valid ? 'Enter 1 to 10080 minutes' : undefined}>
          <Input id="reboot-minutes" autoFocus type="number" min={1} max={10080} value={minutes}
            onChange={(e) => setMinutes(e.target.value)} />
        </FormField>
      </div>
      <p className="clr-secondary" style={{ marginTop: 8 }}>
        Signed-in terminal users get a wall notice; the schedule can be cancelled here
        until it fires.
      </p>
    </Modal>
  );
}

function InstallModal({ open, staged, onClose, onInstalled }) {
  const [reboot, setReboot] = useState(true);
  const [force, setForce] = useState(false);
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (open) { setReboot(true); setForce(false); setError(null); setBusy(false); }
  }, [open]);

  const mismatch = staged && staged.platform_ok === false;

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const r = await api('/api/upgrade/apply', {
        method: 'POST',
        body: JSON.stringify({ reboot, force }),
      });
      onInstalled(r);
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      title="Install Software Image"
      size="sm"
      onClose={busy ? undefined : onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose} disabled={busy}>Cancel</Button>
          <Button variant="danger" onClick={submit} loading={busy}
            disabled={busy || (mismatch && !force)}>
            Install
          </Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      {mismatch && (
        <Alert status="danger" sm style={{ marginBottom: 12 }}>
          This image targets {staged.platform}, which does not match this switch.
          Installing it will almost certainly leave the switch unbootable.
        </Alert>
      )}
      <p>
        Install Hemlock <span className="mono">{staged ? staged.version : ''}</span> over the
        running system? The image replaces the OS on flash; the configuration and user
        accounts in the persist partition are kept. There is no A/B fallback slot — recovery
        from a bad image means reinstalling from ONIE.
      </p>
      {busy && (
        <Alert status="info" sm style={{ marginTop: 12 }}>
          Writing image to flash — this can take a few minutes. Leave this page open.
        </Alert>
      )}
      <div className="clr-form-compact" style={{ marginTop: 12 }}>
        <div className="clr-form-control">
          <Checkbox label="Reboot into the new image after installing"
            checked={reboot} onChange={(e) => setReboot(e.target.checked)} disabled={busy} />
        </div>
        {mismatch && (
          <div className="clr-form-control">
            <Checkbox label="Install anyway (I understand the platform mismatch)"
              checked={force} onChange={(e) => setForce(e.target.checked)} disabled={busy} />
          </div>
        )}
      </div>
    </Modal>
  );
}

/// Ping and traceroute run to completion server-side (a browser has no
/// terminal to stream into) and the whole output lands here.
function DiagnosticsCard({ isAdmin, adminTitle, onError }) {
  const [tool, setTool] = useState('ping');
  const [host, setHost] = useState('');
  const [source, setSource] = useState('');
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState(null);

  const run = async () => {
    setBusy(true);
    setResult(null);
    try {
      const body = JSON.stringify({ host: host.trim(), source: source.trim() });
      const r = await api(`/api/system/diag/${tool}`, { method: 'POST', body });
      setResult(r);
    } catch (err) {
      onError(err.message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card header="Reachability" className="card-wide">
      <CardBlock text="Run ping or traceroute from the switch. The output is collected and shown when the run finishes." />
      <CardBlock>
        <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', alignItems: 'flex-end' }}>
          <Select value={tool} onChange={(e) => setTool(e.target.value)} aria-label="Tool"
            options={[{ value: 'ping', label: 'ping' }, { value: 'traceroute', label: 'traceroute' }]} />
          <Input className="mono" placeholder="host or address" value={host}
            aria-label="Host" onChange={(e) => setHost(e.target.value)} />
          <Input className="mono" placeholder="source interface (optional)" value={source}
            aria-label="Source interface" onChange={(e) => setSource(e.target.value)} />
          <Button variant="primary" sm loading={busy}
            disabled={busy || !host.trim() || !isAdmin} title={adminTitle} onClick={run}>
            Run
          </Button>
        </div>
        {result && (
          <pre className="mono" style={{
            marginTop: 12, padding: 12, maxHeight: 300, overflow: 'auto',
            fontSize: 12, whiteSpace: 'pre-wrap', wordBreak: 'break-word',
          }}>{result.output || '(no output)'}</pre>
        )}
      </CardBlock>
    </Card>
  );
}

/// A TDR sweep interrupts the link, so running one needs the same
/// typed confirmation the CLI asks for. Replaying the last result does
/// not.
function CableCard({ ports, isAdmin, adminTitle, onError }) {
  const [port, setPort] = useState('');
  const [result, setResult] = useState(null);
  const [busy, setBusy] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [typed, setTyped] = useState('');

  const call = async (run) => {
    setBusy(true);
    try {
      const r = await api('/api/system/diag/cable', {
        method: 'POST',
        body: JSON.stringify({ port, run }),
      });
      setResult(r);
    } catch (err) {
      onError(err.message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card header="Cable Diagnostics" className="card-wide">
      <CardBlock text="A time-domain reflectometry sweep reports the state and length of each twisted pair. Copper ports only — and the sweep briefly interrupts the link." />
      <CardBlock>
        <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', alignItems: 'flex-end' }}>
          <Select value={port} onChange={(e) => { setPort(e.target.value); setResult(null); }}
            aria-label="Port"
            options={[{ value: '', label: 'Select a port…' },
              ...ports.map((p) => ({ value: p, label: p }))]} />
          <Button sm disabled={!port || busy} onClick={() => call(false)}>
            Show Last Result
          </Button>
          <Button variant="warning-outline" sm disabled={!port || busy || !isAdmin}
            title={adminTitle} onClick={() => { setTyped(''); setConfirming(true); }}>
            Run Sweep…
          </Button>
        </div>
        {result && !result.has_result && (
          <Alert status="info" sm style={{ marginTop: 12 }}>
            No cable diagnostics have been run on {result.port}.
          </Alert>
        )}
        {result && result.has_result && (
          <Datagrid
            className="diag-grid"
            compact
            rowKey={(r) => r.pair}
            columns={[
              { key: 'pair', label: 'Pair', width: 80 },
              {
                key: 'state', label: 'Status',
                render: (r) => (
                  <Label status={r.state === 'ok' ? 'success' : 'warning'}>{r.state}</Label>
                ),
              },
              {
                key: 'length_m', label: 'Length',
                render: (r) => <span className="cell-mono">{r.length_m ? `${r.length_m} m` : '—'}</span>,
              },
            ]}
            rows={result.pairs}
            footerText={`run ${commitStamp(new Date(result.run_at * 1000).toISOString())}`}
          />
        )}
      </CardBlock>
      <Modal
        open={confirming}
        title={`Run Cable Diagnostics on ${port}`}
        size="sm"
        onClose={() => setConfirming(false)}
        footer={
          <>
            <Button variant="link-neutral" onClick={() => setConfirming(false)}>Cancel</Button>
            <Button variant="warning" loading={busy} disabled={busy || typed.trim() !== 'yes'}
              onClick={() => { setConfirming(false); call(true); }}>
              Run Sweep
            </Button>
          </>
        }
      >
        <p>
          The sweep takes the link on {port} down for a few seconds. Anything behind that
          port loses connectivity for the duration.
        </p>
        <div className="clr-form-compact">
          <FormField label="Confirmation" required htmlFor="cable-confirm"
            helper="Type `yes` to enable the button.">
            <Input id="cable-confirm" autoFocus className="mono" value={typed}
              onChange={(e) => setTyped(e.target.value)} />
          </FormField>
        </div>
      </Modal>
    </Card>
  );
}

/// Collecting a bundle takes a moment; the download link appears when
/// mgmtd says where it landed.
function SupportCard({ isAdmin, adminTitle, onError }) {
  const [busy, setBusy] = useState(false);
  const [bundle, setBundle] = useState(null);

  const collect = async () => {
    setBusy(true);
    setBundle(null);
    try {
      setBundle(await api('/api/system/tech-support', { method: 'POST' }));
    } catch (err) {
      onError(err.message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card header="Tech Support">
      <CardBlock text="Collect the configuration (with secrets redacted), the commit history, daemon state and recent logs into one archive." />
      <CardBlock>
        <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', alignItems: 'center' }}>
          <Button sm icon="bundle" loading={busy} disabled={busy || !isAdmin}
            title={adminTitle} onClick={collect}>
            Collect Bundle
          </Button>
          {bundle && (
            <a className="btn btn-sm btn-primary"
              href={`/api/system/tech-support/download?path=${encodeURIComponent(bundle.path)}`}>
              Download ({formatBytes(bundle.size_bytes)})
            </a>
          )}
        </div>
        {bundle && (
          <div className="dim mono" style={{ marginTop: 8, fontSize: 12 }}>{bundle.path}</div>
        )}
      </CardBlock>
    </Card>
  );
}

/// Regenerating replaces the self-signed pair. Sessions survive — they
/// live in webd memory, not in the TLS material — but the browser sees
/// a new certificate, so the fingerprint is shown to compare against.
function CertificateCard({ isAdmin, adminTitle, onError }) {
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState(null);

  const regenerate = async () => {
    setBusy(true);
    try {
      setResult(await api('/api/system/certificate/regenerate', { method: 'POST' }));
    } catch (err) {
      onError(err.message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card header="Web Certificate">
      <CardBlock text="Replace the self-signed certificate the web console serves. Existing sessions keep working; the browser will warn about the new certificate once the console restarts." />
      <CardBlock>
        <Button sm icon="certificate" variant="warning-outline" loading={busy}
          disabled={busy || !isAdmin} title={adminTitle} onClick={regenerate}>
          Regenerate…
        </Button>
        {result && (
          <div style={{ marginTop: 12 }}>
            <div className="dim" style={{ fontSize: 12 }}>New SHA-256 fingerprint</div>
            <div className="mono" style={{ fontSize: 12, wordBreak: 'break-all' }}>
              {result.fingerprint}
            </div>
          </div>
        )}
      </CardBlock>
    </Card>
  );
}

export default function MaintenancePage() {
  const [data, setData] = useState(null);
  const [error, setError] = useState(null);
  const [notice, setNotice] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null); // 'restore' | 'reboot' | 'schedule' | 'install'
  const [rebooting, setRebooting] = useState(false);
  const [image, setImage] = useState(null);
  const [commits, setCommits] = useState(null);
  const [session, setSession] = useState(null);
  const [rollback, setRollback] = useState(null);
  const [copperPorts, setCopperPorts] = useState([]);
  const [upload, setUpload] = useState(null); // { name, progress } while uploading
  const fileRef = useRef(null);

  const refresh = useCallback(() => {
    api('/api/maintenance').then(setData).catch((e) => setError(e.message));
    api('/api/system/image').then(setImage).catch(() => {});
    api('/api/system/commits').then((r) => setCommits(r.commits)).catch(() => {});
  }, []);
  useEffect(refresh, [refresh]);
  useEffect(() => {
    api('/api/session').then(setSession).catch(() => {});
    // Only twisted-pair ports have pairs to sweep, so the picker offers
    // exactly those — the same rule syncd enforces.
    api('/api/interfaces')
      .then((r) =>
        setCopperPorts(
          r.interfaces
            .filter((i) => i.kind === 'ethernet' && /BASE-?T/i.test(i.media || ''))
            .map((i) => i.name),
        ),
      )
      .catch(() => {});
  }, []);

  const downloadConfig = async () => {
    try {
      const text = await api('/api/config');
      downloadText(`${(data && data.hostname) || 'hemlock'}-${stamp()}.conf`, text);
    } catch (err) {
      setError(err.message);
    }
  };

  const cancelReboot = async () => {
    try {
      await api('/api/reboot/cancel', { method: 'POST' });
      setNotice('Scheduled reboot cancelled.');
      refresh();
    } catch (err) {
      setError(err.message);
    }
  };

  const pickImage = async (e) => {
    const file = e.target.files && e.target.files[0];
    e.target.value = '';
    if (!file) return;
    setError(null);
    setUpload({ name: file.name, progress: 0 });
    try {
      await uploadFile('/api/upgrade/upload', file,
        (p) => setUpload({ name: file.name, progress: p }));
      setNotice(`Image ${file.name} uploaded and staged.`);
      refresh();
    } catch (err) {
      setError(err.message);
    } finally {
      setUpload(null);
    }
  };

  const discardImage = async () => {
    try {
      await api('/api/upgrade/discard', { method: 'POST' });
      setNotice('Staged image discarded.');
      refresh();
    } catch (err) {
      setError(err.message);
    }
  };

  const staged = data && data.staged_image;
  const scheduled = data && data.scheduled_reboot;
  const isAdmin = !session || session.admin;
  const adminTitle = isAdmin ? undefined : 'Operator role: this console is read-only.';

  if (rebooting) {
    return (
      <Shell>
        <RebootingOverlay />
      </Shell>
    );
  }

  return (
    <Shell>
      <div className="page-header">
        <h2>Maintenance</h2>
      </div>
      {error && (
        <Alert status="danger" closable onClose={() => setError(null)} style={{ marginBottom: 16 }}>
          {error}
        </Alert>
      )}
      {notice && (
        <Alert status="success" closable onClose={() => setNotice(null)} style={{ marginBottom: 16 }}>
          {notice}
        </Alert>
      )}
      {applied && applied.length > 0 && (
        <Alert status="success" closable onClose={() => setApplied(null)} items={applied}
          style={{ marginBottom: 16 }} />
      )}
      {scheduled && (
        <Alert status="warning" style={{ marginBottom: 16 }}
          actions={[{ label: 'Cancel Reboot', onClick: cancelReboot }]}>
          {scheduled.mode === 'reboot' ? 'Reboot' : 'Shutdown'} scheduled
          for {formatWhen(scheduled.at_unix)}.
        </Alert>
      )}
      {!data && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {data && (
        <div className="card-grid">
          <Card header="Configuration">
            <CardBlock
              text="Save the running configuration as a file, or restore a previously saved one. A restore is validated by the switch before it replaces the running config." />
            <CardBlock>
              <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                <Button sm icon="download" onClick={downloadConfig}>Download Config</Button>
                <Button sm icon="upload" variant="warning-outline" onClick={() => setModal('restore')}>
                  Restore…
                </Button>
              </div>
            </CardBlock>
          </Card>

          <Card header="Reboot">
            <CardBlock
              text="Reboot the switch immediately, or schedule one for a maintenance window. A scheduled reboot can be cancelled until it fires." />
            <CardBlock>
              <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                <Button sm icon="power" variant="danger-outline" disabled={!isAdmin}
                  title={adminTitle} onClick={() => setModal('reboot')}>
                  Reboot Now
                </Button>
                <Button sm icon="clock" onClick={() => setModal('schedule')}
                  disabled={!!scheduled || !isAdmin} title={adminTitle}>
                  Schedule Reboot…
                </Button>
                <Button sm icon="rescue" variant="danger-outline" disabled={!isAdmin}
                  title={adminTitle} onClick={() => setModal('onie-rescue')}>
                  Reboot into ONIE…
                </Button>
              </div>
            </CardBlock>
          </Card>

          <Card header="Image">
            {!image && <CardBlock text="Reading the installed image…" />}
            {image && (
              <CardBlock>
                <div className="kv">
                  <div className="k">Current Image</div>
                  <div className="v mono">
                    {image.version || '—'}
                    {image.installed_at > 0 && (
                      <span className="dim">
                        {' '}(installed {commitStamp(new Date(image.installed_at * 1000)
                          .toISOString())})
                      </span>
                    )}
                  </div>
                  <div className="k">Image File</div>
                  <div className="v mono">{image.image_file || '—'}</div>
                  <div className="k">Kernel</div>
                  <div className="v mono">{image.kernel || '—'}</div>
                  {image.platform && <div className="k">Platform</div>}
                  {image.platform && <div className="v mono">{image.platform}</div>}
                  <div className="k">Next Boot</div>
                  <div className="v mono">{image.next_boot || '—'}</div>
                  <div className="k">ONIE Rescue</div>
                  <div className="v">
                    {image.onie_rescue_armed
                      ? <Label status="warning" style={MONO}>ARMED</Label>
                      : <Label style={MONO}>NOT ARMED</Label>}
                  </div>
                </div>
              </CardBlock>
            )}
          </Card>

          <Card header="Commit History" className="card-wide">
            {!commits && <CardBlock text="Reading the rollback ring…" />}
            {commits && (
              <Datagrid
                rowKey={(r) => r.index}
                compact
                pageSize={10}
                onRefresh={refresh}
                columns={[
                  {
                    key: 'index', label: 'Idx', width: 60,
                    render: (r) => <span className="cell-mono">{r.index}</span>,
                  },
                  {
                    key: 'time', label: 'Time',
                    render: (r) => <span className="cell-mono">{commitStamp(r.time)}</span>,
                  },
                  { key: 'user', label: 'User', render: (r) => r.user || '—' },
                  {
                    key: 'client', label: 'Client',
                    render: (r) => (r.client ? <Label>{r.client}</Label> : '—'),
                  },
                  {
                    key: 'comment', label: 'Comment',
                    render: (r) =>
                      r.index === 0
                        ? <Label status="success">current</Label>
                        : r.comment || <span className="dim">—</span>,
                  },
                  {
                    key: 'actions', label: '', width: 110,
                    render: (r) =>
                      r.index === 0 ? null : (
                        <Button variant="link" sm icon="undo" disabled={!isAdmin}
                          title={adminTitle}
                          onClick={() => setRollback(r)}>
                          Roll Back
                        </Button>
                      ),
                  },
                ]}
                rows={commits}
                placeholder="No commits recorded yet."
              />
            )}
          </Card>

          <DiagnosticsCard isAdmin={isAdmin} adminTitle={adminTitle} onError={setError} />
          <CableCard ports={copperPorts} isAdmin={isAdmin} adminTitle={adminTitle}
            onError={setError} />
          <SupportCard isAdmin={isAdmin} adminTitle={adminTitle} onError={setError} />
          <CertificateCard isAdmin={isAdmin} adminTitle={adminTitle} onError={setError} />

          <Card header="Software">
            <CardBlock>
              <div className="kv">
                <div className="k">Running Version</div>
                <div className="v mono">{data.version}</div>
                <div className="k">Staged Image</div>
                <div className="v">
                  {staged ? (
                    <>
                      <span className="mono">{staged.version}</span>
                      <span className="dim"> · {formatBytes(staged.size_bytes)}</span>
                      {staged.platform_ok === false && (
                        <>
                          {' '}
                          <Label status="danger" style={MONO}>WRONG PLATFORM</Label>
                        </>
                      )}
                    </>
                  ) : (
                    <span className="dim">None</span>
                  )}
                </div>
              </div>
            </CardBlock>
            <CardBlock>
              <input ref={fileRef} type="file" accept=".bin" hidden onChange={pickImage} />
              {upload ? (
                <div className="progress-block">
                  <span className="progress-label mono">{upload.name}</span>
                  <div className="progress">
                    <div className="progress-fill" style={{ width: `${Math.round(upload.progress * 100)}%` }}></div>
                  </div>
                  <span className="progress-value">{Math.round(upload.progress * 100)}%</span>
                </div>
              ) : staged ? (
                <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
                  <Button sm icon="install" variant="danger-outline" onClick={() => setModal('install')}>
                    Install…
                  </Button>
                  <Button sm icon="trash" variant="link-neutral" onClick={discardImage}>Discard</Button>
                  <Button sm icon="upload-cloud" variant="link-neutral"
                    onClick={() => fileRef.current && fileRef.current.click()}>
                    Replace…
                  </Button>
                </div>
              ) : (
                <Button sm icon="upload-cloud" onClick={() => fileRef.current && fileRef.current.click()}>
                  Upload Image…
                </Button>
              )}
            </CardBlock>
          </Card>
        </div>
      )}

      <RestoreModal
        open={modal === 'restore'}
        onClose={() => setModal(null)}
        onRestored={(changes) => {
          setModal(null);
          setNotice('Configuration restored.');
          setApplied(changes);
          refresh();
        }}
      />
      <RebootNowModal
        open={modal === 'reboot' || modal === 'onie-rescue'}
        onieRescue={modal === 'onie-rescue'}
        onClose={() => setModal(null)}
        onRebooting={() => {
          setModal(null);
          setRebooting(true);
        }}
      />
      <RollbackModal
        open={!!rollback}
        commit={rollback}
        onClose={() => setRollback(null)}
        onRolledBack={(result) => {
          setRollback(null);
          setApplied([
            ...(result.applied.length ? result.applied : ['No changes needed.']),
            ...(result.warnings || []),
          ]);
          refresh();
        }}
      />
      <ScheduleRebootModal
        open={modal === 'schedule'}
        onClose={() => setModal(null)}
        onScheduled={() => {
          setModal(null);
          setNotice('Reboot scheduled.');
          refresh();
        }}
      />
      <InstallModal
        open={modal === 'install'}
        staged={staged}
        onClose={() => setModal(null)}
        onInstalled={(r) => {
          setModal(null);
          if (r.rebooting) {
            setRebooting(true);
          } else {
            setNotice(`Hemlock ${r.version} installed — reboot to run it.`);
            refresh();
          }
        }}
      />
    </Shell>
  );
}
