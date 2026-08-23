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
      else if (body && Array.isArray(body.errors)) message = body.errors.join('; ');
    } catch {
      /* non-JSON error body */
    }
    throw new ApiError(res.status, message);
  }
  if (res.status === 204) return null;
  const type = res.headers.get('content-type') || '';
  return type.includes('application/json') ? res.json() : res.text();
}

// Raw-body file upload with progress callbacks (fetch can't report
// upload progress, so this one path uses XMLHttpRequest). Resolves with
// the parsed JSON response; rejects with ApiError like api().
export function uploadFile(path, file, onProgress) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open('POST', path);
    xhr.withCredentials = true;
    xhr.upload.onprogress = (e) => {
      if (e.lengthComputable && onProgress) onProgress(e.loaded / e.total);
    };
    xhr.onerror = () => reject(new ApiError(0, 'upload failed (network error)'));
    xhr.onload = () => {
      let body = null;
      try {
        body = JSON.parse(xhr.responseText);
      } catch {
        /* non-JSON body */
      }
      if (xhr.status === 401) {
        window.location.assign('/login/');
        reject(new ApiError(401, 'not signed in'));
      } else if (xhr.status >= 200 && xhr.status < 300) {
        resolve(body);
      } else {
        const message =
          (body && body.error) ||
          (body && Array.isArray(body.errors) && body.errors.join('; ')) ||
          xhr.statusText;
        reject(new ApiError(xhr.status, message));
      }
    };
    xhr.setRequestHeader('Content-Type', 'application/octet-stream');
    xhr.send(file);
  });
}

// Trigger a browser download of text content.
export function downloadText(filename, text) {
  const url = URL.createObjectURL(new Blob([text], { type: 'text/plain' }));
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  a.click();
  URL.revokeObjectURL(url);
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

// "Ethernet12" → "Eth12", "Management1" → "Mgmt1" — for dense port lists.
export const shortName = (name) =>
  name.replace(/^Ethernet/, 'Eth').replace(/^Management/, 'Mgmt');

// Natural interface-name order: alphabetic prefix, then numeric suffix
// (Ethernet2 before Ethernet10).
export const compareNames = (a, b) => {
  const parse = (n) => {
    const m = /^(\D*)(\d*)$/.exec(n) || [];
    return [m[1] || n, m[2] ? parseInt(m[2], 10) : -1];
  };
  const [ap, an] = parse(a);
  const [bp, bn] = parse(b);
  return ap === bp ? an - bn : ap < bp ? -1 : 1;
};

// "10,20,30-32" → [10, 20, 30, 31, 32]; throws on junk.
export const parseVlanList = (text) => {
  const out = new Set();
  for (const part of text.split(',').map((p) => p.trim()).filter(Boolean)) {
    const m = /^(\d+)(?:-(\d+))?$/.exec(part);
    if (!m) throw new Error(`bad VLAN list entry "${part}"`);
    const from = parseInt(m[1], 10);
    const to = m[2] ? parseInt(m[2], 10) : from;
    if (from < 1 || to > 4094 || from > to) throw new Error(`bad VLAN range "${part}"`);
    for (let id = from; id <= to; id++) out.add(id);
  }
  return [...out].sort((a, b) => a - b);
};
