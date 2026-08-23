'use client';
import { useEffect } from 'react';
import { useRouter } from 'next/navigation';

// The System tab always opens on its first side-nav entry.
export default function SystemIndex() {
  const router = useRouter();
  useEffect(() => {
    router.replace('/system/general/');
  }, [router]);
  return null;
}
