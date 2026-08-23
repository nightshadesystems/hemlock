// Pin a local copy of Clarity Icons into public/vendor so the console
// works on an air-gapped switch (no CDN). Runs before dev/build.
import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(dirname(fileURLToPath(import.meta.url)));
const src = join(root, 'node_modules', '@clr', 'icons');
const dst = join(root, 'public', 'vendor', 'clr-icons');
mkdirSync(dst, { recursive: true });
for (const f of ['clr-icons.min.js', 'clr-icons.min.css']) {
  copyFileSync(join(src, f), join(dst, f));
}
console.log('vendored @clr/icons ->', dst);
