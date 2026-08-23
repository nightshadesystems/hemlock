'use client';

// Thin fetch wrapper for the hemlock-webd JSON API. Every request is
// same-origin (webd serves both the UI and /api/*); a 401 anywhere
// outside the login page bounces to /login.
export class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

export async function api(path, options = {}) {
  const res = await fetch(path, {
    headers: { 'Content-Type': 'application/json', ...(options.headers || {}) },
    credentials: 'same-origin',
    ...options,
  });
  if (res.status === 401 && !path.startsWith('/api/login')) {
    if (typeof window !== 'undefined' && !window.location.pathname.startsWith('/login')) {
      window.location.assign('/login/');
    }
    throw new ApiError(401, 'not signed in');
  }
  if (!res.ok) {
    let message = res.statusText;
    try {
      const body = await res.json();
      if (body && body.error) message = body.error;
    } catch {
      /* non-JSON error body */
    }
    throw new ApiError(res.status, message);
  }
  if (res.status === 204) return null;
  const type = res.headers.get('content-type') || '';
  return type.includes('application/json') ? res.json() : res.text();
}

export const formatSpeed = (mbps) =>
  !mbps ? '—' : mbps >= 1000 ? `${mbps / 1000} Gb/s` : `${mbps} Mb/s`;

export const formatUptime = (secs) => {
  if (secs == null) return '—';
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return `${d} d ${h} h`;
  if (h > 0) return `${h} h ${m} min`;
  return `${m} min`;
};
