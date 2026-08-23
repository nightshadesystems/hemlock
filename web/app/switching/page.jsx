'use client';
import { useEffect } from 'react';
import { useRouter } from 'next/navigation';

// The Switching tab always opens on its first side-nav entry.
export default function SwitchingIndex() {
  const router = useRouter();
  useEffect(() => {
    router.replace('/switching/interfaces/');
  }, [router]);
  return null;
}
