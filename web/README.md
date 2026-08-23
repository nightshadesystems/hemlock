# Hemlock web console

The switch's browser UI: a Next.js app exported to static files and
served by `hemlock-webd` (pure Rust, axum + rustls) together with the
JSON API under `/api/*`. Styling is the Nightshade Clarity design system
(`styles/` tokens + framework, `components/ds/` React components),
self-hosted end to end — fonts via @fontsource, Clarity Icons pinned
into `public/vendor/` — so the console works on an air-gapped switch.

## On the switch

The console is config-driven, like SSH:

    set system http        # listen on :80
    set system https       # listen on :443 — a self-signed certificate
                           # is generated on first start and persisted
    commit

Sign in with a switch operator account (a member of the `hemlock`
group, e.g. `admin`). mgmtd starts/stops `hemlock-webd.service` on
commit and boot replay; webd reads the running config to pick its
listeners.

## Development

    npm install
    npm run build          # static export to web/out/

Run the stack locally (mock dataplane, TCP loopback IPC):

    cargo run -p hemlock-syncd -- --platform cel-e1031 --mock
    cargo run -p hemlock-mgmtd
    cargo run -p hemlock-webd -- --http-port 8080 --dev-listen http \
        --dev-auth admin:admin --assets web/out

then open http://localhost:8080. For UI iteration with hot reload, run
`npm run dev` instead — it proxies `/api/*` to webd on port 8080
(override with `HEMLOCK_WEBD_URL`).

`mkimage.sh` installs `web/out` at `/usr/share/hemlock/web` in the
image, building it first when missing.
