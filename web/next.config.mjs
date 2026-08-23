/** @type {import('next').NextConfig} */
const dev = process.env.NODE_ENV === 'development';

// Static export: hemlock-webd serves web/out and the /api/* endpoints
// from one origin. `next dev` has no webd behind it, so proxy /api/*
// to a locally running hemlock-webd (rewrites are dev-only; they are
// not part of the exported output).
const nextConfig = {
  ...(dev
    ? {
        async rewrites() {
          return [
            {
              source: '/api/:path*',
              destination:
                (process.env.HEMLOCK_WEBD_URL || 'http://127.0.0.1:8080') + '/api/:path*',
            },
          ];
        },
      }
    : { output: 'export' }),
  trailingSlash: true,
  images: { unoptimized: true },
};

export default nextConfig;
