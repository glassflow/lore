// Checks the pure functions of the admin page (the `lore-admin-pure` script block of
// src/web/admin.html) with node. Run by tests/web.rs, with TZ=UTC:
// node tests/web_admin_pure.js src/web/admin.html
'use strict';
const assert = require('assert');
const fs = require('fs');

const html = fs.readFileSync(process.argv[2], 'utf8');
const m = /<script id="lore-admin-pure">([\s\S]*?)<\/script>/.exec(html);
assert.ok(m, 'the page has no lore-admin-pure script block');

// The block is evaluated on its own, with `sessionStorage` as a parameter so a test can swap it.
function load(storage) {
  return new Function(
    'sessionStorage',
    m[1] +
      '\nreturn { loadToken, saveToken, errorMessage, formatCount, plural, shortSha, commitText,' +
      ' ago, formatBytes, formatDuration, formatDate, isRunning, anyRunning, pollDelay, pushEnded,' +
      ' progressText, progressFraction, stateClass, pushTitle, embedText, pushTiming, scopesFrom,' +
      ' keyActive, tenantSummary, modelText, serverFacts, storageSummary, indexKind, keysUrl,' +
      ' revokeUrl };'
  )(storage);
}

const throwing = {
  getItem() { throw new Error('blocked'); },
  setItem() { throw new Error('blocked'); },
  removeItem() { throw new Error('blocked'); },
};
const page = load(throwing);
let count = 0;
function test(name, fn) {
  try {
    fn();
    count += 1;
  } catch (e) {
    console.error('FAIL ' + name + ': ' + e.message);
    process.exitCode = 1;
  }
}

test('the token helpers survive storage that throws', () => {
  assert.strictEqual(page.loadToken(), '');
  page.saveToken('secret');
  page.saveToken('');
});

test('the token is kept under lore.admin in session storage', () => {
  const data = {};
  const ok = load({
    getItem: (k) => (k in data ? data[k] : null),
    setItem: (k, v) => { data[k] = v; },
    removeItem: (k) => { delete data[k]; },
  });
  ok.saveToken('secret');
  assert.deepStrictEqual(data, { 'lore.admin': 'secret' });
  assert.strictEqual(ok.loadToken(), 'secret');
  ok.saveToken('');
  assert.deepStrictEqual(data, {});
  assert.strictEqual(ok.loadToken(), '');
});

test('errorMessage explains a wrong token and a disabled admin API', () => {
  assert.strictEqual(page.errorMessage(401, '{"error":"unauthorized"}'), 'Wrong or missing admin token');
  assert.strictEqual(
    page.errorMessage(403, '{"error":"admin API disabled"}'),
    'The admin API is off on this server: set LORE_ADMIN_TOKEN to turn it on'
  );
  assert.strictEqual(page.errorMessage(404, '{"error":"not found"}'), 'not found');
  assert.strictEqual(page.errorMessage(502, '<html>Bad   Gateway</html>'), 'Request failed (HTTP 502): <html>Bad Gateway</html>');
  assert.strictEqual(page.errorMessage(500, ''), 'Request failed (HTTP 500)');
});

test('counts get thousands separators whatever the locale', () => {
  assert.strictEqual(page.formatCount(0), '0');
  assert.strictEqual(page.formatCount(999), '999');
  assert.strictEqual(page.formatCount(1234), '1,234');
  assert.strictEqual(page.formatCount(1234567), '1,234,567');
  assert.strictEqual(page.formatCount(null), '0');
  assert.strictEqual(page.formatCount(undefined), '0');
  assert.strictEqual(page.plural(1, 'file', 'files'), '1 file');
  assert.strictEqual(page.plural(2000, 'file', 'files'), '2,000 files');
  assert.strictEqual(page.plural(0, 'file', 'files'), '0 files');
});

test('commits are shortened, and only repos say "not indexed"', () => {
  const sha = '0123456789abcdef0123456789abcdef01234567';
  assert.strictEqual(page.shortSha(sha), '0123456');
  assert.strictEqual(page.shortSha(null), '');
  assert.strictEqual(page.commitText({ kind: 'repo', indexed_commit: sha }), '0123456');
  assert.strictEqual(page.commitText({ kind: 'repo', indexed_commit: null }), 'not indexed');
  assert.strictEqual(page.commitText({ kind: 'documents', indexed_commit: null }), '');
});

test('ago is relative to the server clock', () => {
  const now = '2026-11-03T12:00:00Z';
  assert.strictEqual(page.ago('2026-11-03T12:00:00Z', now), 'just now');
  assert.strictEqual(page.ago('2026-11-03T11:59:18Z', now), '42 s ago');
  assert.strictEqual(page.ago('2026-11-03T11:55:00Z', now), '5 min ago');
  assert.strictEqual(page.ago('2026-11-03T09:00:00Z', now), '3 h ago');
  assert.strictEqual(page.ago('2026-10-31T12:00:00Z', now), '3 days ago');
  // A time ahead of the server clock is "just now", and bad input is empty.
  assert.strictEqual(page.ago('2026-11-03T12:00:30Z', now), 'just now');
  assert.strictEqual(page.ago(null, now), '');
  assert.strictEqual(page.ago('nonsense', now), '');
});

test('sizes are in powers of 1024', () => {
  assert.strictEqual(page.formatBytes(0), '0 B');
  assert.strictEqual(page.formatBytes(1023), '1023 B');
  assert.strictEqual(page.formatBytes(1536), '1.5 KiB');
  assert.strictEqual(page.formatBytes(23 * 1024 * 1024), '23 MiB');
  assert.strictEqual(page.formatBytes(1.25 * 1024 * 1024 * 1024), '1.3 GiB');
  assert.strictEqual(page.formatBytes(null), '');
  assert.strictEqual(page.formatBytes(-1), '');
  assert.strictEqual(
    page.storageSummary({ database_bytes: 3 * 1024 * 1024, repos_bytes: 512 }),
    'Database 3.0 MiB · repos on disk 512 B'
  );
  assert.strictEqual(page.indexKind('hnsw'), 'vectors (hnsw)');
  assert.strictEqual(page.indexKind('gin'), 'full-text (gin)');
  assert.strictEqual(page.indexKind('btree'), 'btree');
});

test('durations and dates', () => {
  assert.strictEqual(page.formatDuration(8400), '8.4 s');
  assert.strictEqual(page.formatDuration(59999), '60.0 s');
  assert.strictEqual(page.formatDuration(134000), '2 min 14 s');
  assert.strictEqual(page.formatDuration(3780000), '1 h 3 min');
  assert.strictEqual(page.formatDuration(-5), '0.0 s');
  assert.strictEqual(page.formatDate('2026-11-03T14:05:59Z'), '2026-11-03 14:05');
  assert.strictEqual(page.formatDate(null), '');
});

test('polling is quick only while a push runs', () => {
  assert.strictEqual(page.isRunning({ state: 'waiting' }), true);
  assert.strictEqual(page.isRunning({ state: 'indexing' }), true);
  assert.strictEqual(page.isRunning({ state: 'done' }), false);
  assert.strictEqual(page.isRunning({ state: 'failed' }), false);
  assert.strictEqual(page.anyRunning(null), false);
  assert.strictEqual(page.pollDelay({ running: [{ id: 1 }], recent: [] }), 3000);
  assert.strictEqual(page.pollDelay({ running: [], recent: [{ id: 1 }] }), 30000);
  assert.strictEqual(page.pollDelay(null), 30000);
});

test('pushEnded notices a push that left the running list', () => {
  const one = { running: [{ id: 1 }] };
  assert.strictEqual(page.pushEnded(null, one), false);
  assert.strictEqual(page.pushEnded(one, one), false);
  assert.strictEqual(page.pushEnded(one, { running: [{ id: 2 }, { id: 1 }] }), false);
  assert.strictEqual(page.pushEnded(one, { running: [] }), true);
  assert.strictEqual(page.pushEnded({ running: [] }, { running: [] }), false);
  assert.strictEqual(page.pushEnded(one, {}), true);
});

test('progress text and fraction for each state', () => {
  assert.strictEqual(page.progressText({ state: 'waiting' }), 'waiting for another push to this source');
  assert.strictEqual(page.progressText({ state: 'fetching', files_total: null }), 'fetching the bundle');
  const half = { state: 'indexing', files_total: 1816, files_done: 908 };
  assert.strictEqual(page.progressText(half), '908 of 1,816 files');
  assert.strictEqual(page.progressFraction(half), 0.5);
  assert.strictEqual(page.progressText({ state: 'done', files_total: 1, files_done: 1 }), '1 file');
  assert.strictEqual(page.progressFraction({ state: 'done', files_total: null, files_done: 0 }), 1);
  assert.strictEqual(page.progressText({ state: 'failed', error: 'need a full bundle' }), 'need a full bundle');
  assert.strictEqual(page.progressFraction({ state: 'fetching', files_total: null }), null);
  assert.strictEqual(page.progressFraction({ state: 'indexing', files_total: 0, files_done: 0 }), 1);
  assert.strictEqual(page.progressFraction({ state: 'failed', files_total: 4, files_done: 1 }), 0.25);
  assert.deepStrictEqual(
    ['waiting', 'fetching', 'indexing', 'done', 'failed'].map((s) => page.stateClass({ state: s })),
    ['off', 'run', 'run', 'ok', 'bad']
  );
});

test('a push is titled tenant / source and timed against the server clock', () => {
  assert.strictEqual(page.pushTitle({ tenant: 'acme', source: 'sample' }), 'acme / sample');
  assert.strictEqual(page.pushTitle({ tenant: null, source: 'sample' }), 'deleted tenant / sample');
  const now = '2026-11-03T12:10:00Z';
  const running = { started_at: '2026-11-03T12:09:48Z', finished_at: null };
  assert.strictEqual(page.pushTiming(running, now), 'running for 12.0 s');
  const ended = { started_at: '2026-11-03T12:00:00Z', finished_at: '2026-11-03T12:01:40Z' };
  assert.strictEqual(page.pushTiming(ended, now), 'took 1 min 40 s');
});

test('embedText says how many vectors were computed, kept, and how long it took', () => {
  assert.strictEqual(page.embedText({ chunks: 0, chunks_embedded: 0, embed_ms: 0 }), 'none');
  assert.strictEqual(
    page.embedText({ chunks: 1652, chunks_embedded: 1652, embed_ms: 9800 }),
    '1,652 new in 9.8 s'
  );
  assert.strictEqual(
    page.embedText({ chunks: 40, chunks_embedded: 3, embed_ms: 250 }),
    '3 new, 37 reused in 0.3 s'
  );
});

test('scopes come out in the server order, and none is refused', () => {
  assert.deepStrictEqual(page.scopesFrom(true, true), ['ingest', 'read']);
  assert.deepStrictEqual(page.scopesFrom(true, false), ['read']);
  assert.deepStrictEqual(page.scopesFrom(false, true), ['ingest']);
  assert.strictEqual(page.scopesFrom(false, false), null);
});

test('a tenant summary counts its sources and active keys', () => {
  const t = {
    sources: [{ documents: 1200, chunks: 9000 }, { documents: 34, chunks: 876 }],
    keys: [{ revoked_at: null }, { revoked_at: '2026-11-01T00:00:00Z' }],
  };
  assert.strictEqual(page.tenantSummary(t), '2 sources · 1,234 documents · 9,876 chunks · 1 active key');
  assert.strictEqual(page.tenantSummary({}), '0 sources · 0 documents · 0 chunks · 0 active keys');
  assert.strictEqual(page.keyActive({ revoked_at: null }), true);
  assert.strictEqual(page.keyActive({ revoked_at: '2026-11-01T00:00:00Z' }), false);
});

test('server facts name the models and say when deep mode is off', () => {
  const facts = page.serverFacts({
    version: '0.1.0',
    llm: 'anthropic',
    embedder: 'fastembed:bge-small-en-v1.5',
    modes: ['fast', 'deep'],
    models: { fast: { model: 'm-fast', effort: 'low' }, deep: { model: 'm-deep', effort: null } },
    tenants: 2,
    sources: 3,
    documents: 1234,
    chunks: 56789,
  });
  assert.deepStrictEqual(facts, [
    ['Version', '0.1.0'],
    ['Embedder', 'fastembed:bge-small-en-v1.5'],
    ['Model provider', 'anthropic'],
    ['Fast answers', 'm-fast (low effort)'],
    ['Deep answers', 'm-deep'],
    ['Tenants', '2'],
    ['Sources', '3'],
    ['Documents', '1,234'],
    ['Chunks', '56,789'],
  ]);
  const fastOnly = page.serverFacts({ modes: ['fast'], models: { fast: { model: 'q' }, deep: { model: 'q' } } });
  assert.deepStrictEqual(fastOnly[4], ['Deep answers', 'off']);
  // Memory is shown when the server knows it (Linux).
  const linux = page.serverFacts({ memory: { rss_bytes: 400 * 1024 * 1024, peak_rss_bytes: 3.75 * 1024 * 1024 * 1024 } });
  assert.deepStrictEqual(linux.slice(5, 7), [['Memory', '400 MiB'], ['Peak memory', '3.8 GiB']]);
  assert.strictEqual(linux.length, 11);
  assert.strictEqual(page.serverFacts({ memory: { rss_bytes: null, peak_rss_bytes: null } }).length, 9);
  assert.strictEqual(page.modelText(null), '');
});

test('key URLs escape their ids', () => {
  assert.strictEqual(page.keysUrl('abc'), '/v1/admin/tenants/abc/keys');
  assert.strictEqual(page.keysUrl('a/../b'), '/v1/admin/tenants/a%2F..%2Fb/keys');
  assert.strictEqual(page.revokeUrl('k?x=1'), '/v1/admin/keys/k%3Fx%3D1');
});

if (!process.exitCode) console.log('web_admin_pure: ' + count + ' checks passed');
