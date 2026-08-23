'use client';
import { useEffect } from 'react';
import { useRouter } from 'next/navigation';
import { api } from '@/lib/api';

// Entry point: land on the dashboard when signed in, /login otherwise
// (the api helper redirects on 401).
export default function Index() {
  const router = useRouter();
  useEffect(() => {
    api('/api/session')
      .then(() => router.replace('/dashboard/'))
      .catch(() => {});
  }, [router]);
  return <div className="page-loading"><span className="spinner spinner-md"></span>Loading…</div>;
}
