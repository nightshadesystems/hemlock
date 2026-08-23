'use client';
import { Label } from '@/components/ds/misc';

const MONO = { fontFamily: 'var(--ns-font-mono)', letterSpacing: '0.06em' };

export function OperLabel({ up }) {
  return (
    <Label status={up ? 'success' : 'danger'} style={MONO}>
      {up ? 'UP' : 'DOWN'}
    </Label>
  );
}

export function AdminLabel({ up }) {
  return up ? (
    <Label status="success" style={MONO}>ENABLED</Label>
  ) : (
    <Label style={MONO}>SHUTDOWN</Label>
  );
}

export function ServiceLabel({ on }) {
  return (
    <Label status={on ? 'success' : undefined} style={MONO}>
      {on ? 'ENABLED' : 'DISABLED'}
    </Label>
  );
}

export function ModeLabel({ iface }) {
  if (iface.kind === 'management') return <Label>Management</Label>;
  if (iface.kind === 'vlan') return <Label>SVI</Label>;
  if (iface.addresses && iface.addresses.length > 0) return <Label accent>Routed</Label>;
  if (iface.switchport_mode === 'trunk') return <Label>Trunk</Label>;
  return <Label>Access</Label>;
}
