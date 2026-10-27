#!/usr/bin/env node
import { readFileSync } from 'node:fs';
import { mkdir, writeFile } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const asset = (path) => readFileSync(new URL(`../shared/assets/${path}`, import.meta.url)).toString('base64');
const fonts = {
  sans: asset('fonts/dm-sans-latin-variable.woff2'),
  heading: asset('fonts/manrope-latin-variable.woff2'),
};
const strata = asset('rise-strata.svg');
const escapeHtml = (value) => value.replace(/[&<>"']/g, (char) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[char]);

function docsPath(value) {
  if (typeof value !== 'string' || !/^\/(?!\/)/.test(value)) {
    throw new TypeError('Documentation URLs must be root-relative paths.');
  }
  return escapeHtml(value);
}

export function renderDocsIndex(userUrl, operatorUrl) {
  const userHref = docsPath(userUrl);
  const operatorHref = docsPath(operatorUrl);
  return `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <meta name="color-scheme" content="light dark">
  <title>Rise documentation</title>
  <style>
    @font-face { font-family: RiseSans; font-style: normal; font-weight: 400 700; font-display: swap; src: url(data:font/woff2;base64,${fonts.sans}) format('woff2'); }
    @font-face { font-family: RiseHeading; font-style: normal; font-weight: 400 800; font-display: swap; src: url(data:font/woff2;base64,${fonts.heading}) format('woff2'); }
    :root { color-scheme: light dark; --paper:#fbfbfe; --ink:#232337; --muted:#62647a; --line:#27274b18; --accent:#5146c4; --tint:#ecebfc; --glow:#c8baf34d; --glow-secondary:#b4d8ff3b; font-family:RiseSans,system-ui,sans-serif; color:var(--ink); background:var(--paper); }
    * { box-sizing:border-box; }
    body { margin:0; min-height:100svh; display:grid; place-items:center; padding:48px 24px; background:radial-gradient(ellipse 68rem 23rem at 72% 0,var(--glow),transparent),radial-gradient(ellipse 42rem 20rem at 5% 0,var(--glow-secondary),transparent),var(--paper); -webkit-font-smoothing:antialiased; }
    main { width:100%; max-width:680px; }
    .brand { display:flex; align-items:center; gap:12px; margin-bottom:40px; }
    .strata { width:30px; height:30px; background:var(--accent); mask:url(data:image/svg+xml;base64,${strata}) center/contain no-repeat; -webkit-mask:url(data:image/svg+xml;base64,${strata}) center/contain no-repeat; }
    .wordmark { font-family:RiseHeading,system-ui,sans-serif; font-size:30px; font-weight:800; line-height:1; letter-spacing:-.09em; }
    .wordmark span { color:var(--accent); }
    h1 { font-family:RiseHeading,system-ui,sans-serif; font-size:32px; line-height:1.3; font-weight:750; letter-spacing:-.04em; margin:0 0 16px; }
    p { margin:0 0 32px; color:var(--muted); font-size:16px; line-height:1.8; }
    ul { margin:0; padding:0; list-style:none; border-bottom:1px solid var(--line); }
    li { border-top:1px solid var(--line); }
    a { display:grid; grid-template-columns:1fr auto; gap:8px 24px; padding:24px 12px; color:inherit; text-decoration:none; border-radius:8px; }
    a:hover { background:var(--tint); }
    a:focus-visible { outline:2px solid var(--accent); outline-offset:4px; }
    strong { color:var(--accent); font-size:17px; font-weight:600; }
    .description { grid-column:1; color:var(--muted); font-size:14px; line-height:1.8; }
    .arrow { grid-column:2; grid-row:1/3; align-self:center; color:var(--accent); font-size:20px; }
    @media(prefers-color-scheme:dark) { :root { --paper:#13141c; --ink:#ececf5; --muted:#a2a5bb; --line:#d0d0ff19; --accent:#aaa2ff; --tint:#282640; --glow:#5c479b30; --glow-secondary:#394b8b22; } }
  </style>
</head>
<body>
  <main>
    <div class="brand" role="img" aria-label="Rise"><span class="strata" aria-hidden="true"></span><span class="wordmark" aria-hidden="true">rise<span>.</span></span></div>
    <h1>Documentation</h1>
    <p>Choose the documentation for your work with Rise.</p>
    <ul aria-label="Documentation sites">
      <li><a href="${userHref}"><strong>User docs</strong><span class="description">Build and deploy applications, configure projects, and use platform integrations.</span><span class="arrow" aria-hidden="true">→</span></a></li>
      <li><a href="${operatorHref}"><strong>Operator docs</strong><span class="description">Install and operate Rise, configure extension providers, and contribute to the platform.</span><span class="arrow" aria-hidden="true">→</span></a></li>
    </ul>
  </main>
</body>
</html>
`;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [outputPath, userUrl, operatorUrl, ...extra] = process.argv.slice(2);
  if (!outputPath || !userUrl || !operatorUrl || extra.length) {
    console.error('Usage: node docs/scripts/render-docs-index.mjs <output-file> <user-url> <operator-url>');
    process.exitCode = 1;
  } else {
    await mkdir(dirname(outputPath), { recursive: true });
    await writeFile(outputPath, renderDocsIndex(userUrl, operatorUrl));
  }
}
