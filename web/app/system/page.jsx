'use client';
import { useEffect, useState } from 'react';
import Shell from '@/components/Shell';
import { api, formatUptime } from '@/lib/api';
import { Alert, Card, CardBlock } from '@/components/ds/misc';
import { ServiceLabel } from '@/components/status';

import { Fragment } from 'react';

function KV({ rows }) {
  return (
    <div className="kv">
      {rows.map(([k, v]) => (
        <Fragment key={k}>
          <div className="k">{k}</div>
          <div className="v">{v}</div>
        </Fragment>
      ))}
    </div>
  );
}

export default function SystemPage() {
  const [system, setSystem] = useState(null);
  const [config, setConfig] = useState(null);
  const [error, setError] = useState(null);

  useEffect(() => {
    Promise.all([api('/api/system'), api('/api/config')])
      .then(([s, c]) => {
        setSystem(s);
        setConfig(c);
      })
      .catch((e) => setError(e.message));
  }, []);

  return (
    <Shell>
      <div className="page-header">
        <h2>System</h2>
      </div>
      {error && <Alert status="danger" style={{ marginBottom: 16 }}>{error}</Alert>}
      {!system && !error && (
        <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>
      )}
      {system && (
        <>
          <div className="card-grid" style={{ marginBottom: 24 }}>
            <Card header="Identity">
              <CardBlock>
                <KV
                  rows={[
                    ['Hostname', <span className="mono" key="h">{system.hostname}</span>],
                    ['Version', <span className="mono" key="v">v{system.version}</span>],
                    ['Platform', system.platform_model || system.platform_id || '—'],
                    ['Dataplane', <span className="mono" key="b">{system.backend}</span>],
                    ['Ports', <span className="mono" key="p">{system.port_count}</span>],
                    ['Uptime', formatUptime(system.uptime_secs)],
                  ]}
                />
              </CardBlock>
            </Card>
            <Card header="Services">
              <CardBlock>
                <KV
                  rows={[
                    ['SSH', <ServiceLabel key="ssh" on={system.services.ssh} />],
                    ['HTTP', <ServiceLabel key="http" on={system.services.http} />],
                    ['HTTPS', <ServiceLabel key="https" on={system.services.https} />],
                  ]}
                />
                <p className="clr-caption" style={{ marginTop: 12 }}>
                  Services follow the committed configuration: set system ssh · set system http ·
                  set system https
                </p>
              </CardBlock>
            </Card>
          </div>
          <h3 className="clr-subsection" style={{ marginBottom: 8 }}>Running configuration</h3>
          <pre>{config && config.trim() ? config : '(empty configuration)'}</pre>
        </>
      )}
    </Shell>
  );
}
