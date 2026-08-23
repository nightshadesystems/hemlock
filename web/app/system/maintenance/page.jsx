'use client';
import { useCallback, useEffect, useRef, useState } from 'react';
import Shell from '@/components/Shell';
import { api, uploadFile, downloadText } from '@/lib/api';
import { Alert, Card, CardBlock, Label } from '@/components/ds/misc';
import { Button } from '@/components/ds/Button';
import { Modal } from '@/components/ds/Modal';
import { FormField, Input, Checkbox } from '@/components/ds/forms';

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

function RebootNowModal({ open, onClose, onRebooting }) {
  const [error, setError] = useState(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (open) { setError(null); setBusy(false); }
  }, [open]);

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      await api('/api/reboot', { method: 'POST', body: JSON.stringify({ in_minutes: 0 }) });
      onRebooting();
    } catch (err) {
      setError(err.message);
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      title="Reboot Switch"
      size="sm"
      onClose={onClose}
      footer={
        <>
          <Button variant="link-neutral" onClick={onClose}>Cancel</Button>
          <Button variant="danger" onClick={submit} loading={busy} disabled={busy}>Reboot Now</Button>
        </>
      }
    >
      {error && <Alert status="danger" sm style={{ marginBottom: 12 }}>{error}</Alert>}
      <p>
        Reboot the switch immediately? All ports go down until the switch has booted and
        reconverged, and unsaved candidate changes are discarded.
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

export default function MaintenancePage() {
  const [data, setData] = useState(null);
  const [error, setError] = useState(null);
  const [notice, setNotice] = useState(null);
  const [applied, setApplied] = useState(null);
  const [modal, setModal] = useState(null); // 'restore' | 'reboot' | 'schedule' | 'install'
  const [rebooting, setRebooting] = useState(false);
  const [upload, setUpload] = useState(null); // { name, progress } while uploading
  const fileRef = useRef(null);

  const refresh = useCallback(() => {
    api('/api/maintenance').then(setData).catch((e) => setError(e.message));
  }, []);
  useEffect(refresh, [refresh]);

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
                <Button sm icon="power" variant="danger-outline" onClick={() => setModal('reboot')}>
                  Reboot Now
                </Button>
                <Button sm icon="clock" onClick={() => setModal('schedule')} disabled={!!scheduled}>
                  Schedule Reboot…
                </Button>
              </div>
            </CardBlock>
          </Card>

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
        open={modal === 'reboot'}
        onClose={() => setModal(null)}
        onRebooting={() => {
          setModal(null);
          setRebooting(true);
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
