'use client';
import { useEffect } from 'react';
import { useRouter } from 'next/navigation';

// The QoS tab always opens on its first side-nav entry.
export default function QosIndex() {
  const router = useRouter();
  useEffect(() => {
    router.replace('/qos/maps/');
  }, [router]);
  return null;
}
