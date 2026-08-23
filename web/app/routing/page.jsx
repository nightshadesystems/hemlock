'use client';
import { useEffect } from 'react';
import { useRouter } from 'next/navigation';

// The Routing tab always opens on its first side-nav entry.
export default function RoutingIndex() {
  const router = useRouter();
  useEffect(() => {
    router.replace('/routing/static/');
  }, [router]);
  return null;
}
