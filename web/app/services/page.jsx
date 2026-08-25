'use client';
import { useEffect } from 'react';
import { useRouter } from 'next/navigation';

// The Services tab always opens on its first side-nav entry.
export default function ServicesIndex() {
  const router = useRouter();
  useEffect(() => {
    router.replace('/services/lldp/');
  }, [router]);
  return null;
}
