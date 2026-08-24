'use client';
import { useEffect } from 'react';
import { useRouter } from 'next/navigation';

// The Security tab always opens on its first side-nav entry.
export default function SecurityIndex() {
  const router = useRouter();
  useEffect(() => {
    router.replace('/security/acls/');
  }, [router]);
  return null;
}
