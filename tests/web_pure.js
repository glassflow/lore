// Checks the pure functions of the web page (the `lore-pure` script block of
// src/web/index.html) with node. Run by tests/web.rs: node tests/web_pure.js src/web/index.html
'use strict';
const assert = require('assert');
const fs = require('fs');

const html = fs.readFileSync(process.argv[2], 'utf8');
const m = /<script id="lore-pure">([\s\S]*?)<\/script>/.exec(html);
assert.ok(m, 'the page has no lore-pure script block');

// The block is evaluated on its own, with `localStorage` as a parameter so a test can swap it.
function load(storage) {
  return new Function(
    'localStorage',
    m[1] +
      '\nreturn { loadKey, saveKey, escapeHtml, safeUrl, renderAnswer, citationView,' +
      ' footerText, errorMessage, deepAvailable, shouldSubmit };'
  )(storage);
}

const throwing = {
  getItem() { throw new Error('blocked'); },
  setItem() { throw new Error('blocked'); },
  removeItem() { throw new Error('blocked'); },
};
const lore = load(throwing);
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

test('escapeHtml escapes the five characters', () => {
  assert.strictEqual(lore.escapeHtml('<a href="x">&\'</a>'), '&lt;a href=&quot;x&quot;&gt;&amp;&#39;&lt;/a&gt;');
});

test('script tags in the answer are escaped', () => {
  const out = lore.renderAnswer('<script>alert(1)</script>', []);
  assert.ok(!out.includes('<script'), out);
  assert.ok(out.includes('&lt;script&gt;'), out);
});

test('attribute injection through the answer and through a label is escaped', () => {
  let out = lore.renderAnswer('<img src=x onerror=alert(1)> and "quoted"', []);
  assert.ok(!out.includes('<img'), out);
  out = lore.renderAnswer('see ["><b>x]', [{ label: '"><b>x' }]);
  assert.ok(!out.includes('<b>'), out);
});

test('blank-line separated blocks become paragraphs', () => {
  assert.strictEqual(lore.renderAnswer('first\n\nsecond', []), '<p>first</p><p>second</p>');
  assert.strictEqual(lore.renderAnswer('a\r\n\r\nb', []), '<p>a</p><p>b</p>');
});

test('lines starting with "- " or "* " become a list', () => {
  assert.strictEqual(lore.renderAnswer('- one\n* two', []), '<ul><li>one</li><li>two</li></ul>');
  assert.strictEqual(lore.renderAnswer('intro\n- one', []), '<p>intro</p><ul><li>one</li></ul>');
});

test('backticks become code, with the content escaped', () => {
  assert.strictEqual(lore.renderAnswer('use `x<y` now', []), '<p>use <code>x&lt;y</code> now</p>');
});

test('labels that match a citation become links, others stay text', () => {
  const out = lore.renderAnswer('A [1] and [2], not [9] or [x].', [{ label: '1' }, { label: '2' }]);
  assert.ok(out.includes('<a href="#cite-0">[1]</a>'), out);
  assert.ok(out.includes('<a href="#cite-1">[2]</a>'), out);
  assert.ok(out.includes('not [9] or [x].'), out);
});

test('labels inside code spans are not linked', () => {
  assert.strictEqual(lore.renderAnswer('`[1]`', [{ label: '1' }]), '<p><code>[1]</code></p>');
});

test('odd labels do not break rendering', () => {
  const out = lore.renderAnswer('[__proto__] [constructor] [toString]', [{ label: '1' }]);
  assert.strictEqual(out, '<p>[__proto__] [constructor] [toString]</p>');
  assert.strictEqual(lore.renderAnswer('x', undefined), '<p>x</p>');
});

test('safeUrl follows http and https only', () => {
  assert.strictEqual(lore.safeUrl('http://a.test/x'), 'http://a.test/x');
  assert.strictEqual(lore.safeUrl('HTTPS://a.test/x'), 'HTTPS://a.test/x');
  for (const bad of ['javascript:alert(1)', 'JaVaScRiPt:alert(1)', 'data:text/html,x', 'ftp://a.test',
    '//evil.test', '/relative', ' http://a.test', '', null, undefined, 5]) {
    assert.strictEqual(lore.safeUrl(bad), null, String(bad));
  }
});

test('citationView shows path:start-end, falls back to title, and links only safe urls', () => {
  const code = lore.citationView({ label: '1', kind: 'code', path: 'src/a.rs', start_line: 3, end_line: 9, url: 'https://h.test/a' });
  assert.deepStrictEqual(code, { text: '[1] src/a.rs:3-9', href: 'https://h.test/a', kind: 'code' });
  const session = lore.citationView({ label: '2', kind: 'session', path: null, title: 'Standup chat', url: null });
  assert.deepStrictEqual(session, { text: '[2] Standup chat', href: null, kind: 'session' });
  assert.strictEqual(lore.citationView({ label: '3', path: 'p.md' }).text, '[3] p.md');
  assert.strictEqual(lore.citationView({ label: '4', kind: 'note' }).text, '[4] note');
  assert.strictEqual(lore.citationView({ label: '5', path: 'a', url: 'javascript:alert(1)' }).href, null);
});

test('the key helpers survive storage that throws', () => {
  assert.strictEqual(lore.loadKey(), '');
  lore.saveKey('lk_abc');
  lore.saveKey('');
});

test('the key is kept under lore.key when storage works', () => {
  const data = {};
  const ok = load({
    getItem: (k) => (k in data ? data[k] : null),
    setItem: (k, v) => { data[k] = v; },
    removeItem: (k) => { delete data[k]; },
  });
  ok.saveKey('lk_abc');
  assert.deepStrictEqual(data, { 'lore.key': 'lk_abc' });
  assert.strictEqual(ok.loadKey(), 'lk_abc');
  ok.saveKey('');
  assert.strictEqual(ok.loadKey(), '');
});

test('errorMessage prefers the server error and handles non-JSON bodies', () => {
  assert.strictEqual(lore.errorMessage(401, '{"error":"unauthorized"}'), 'Invalid or missing key');
  assert.strictEqual(lore.errorMessage(401, ''), 'Invalid or missing key');
  assert.strictEqual(lore.errorMessage(400, '{"error":"question is empty"}'), 'question is empty');
  assert.strictEqual(lore.errorMessage(422, '{"error":"the model declined to answer"}'), 'the model declined to answer');
  assert.strictEqual(lore.errorMessage(502, '<html>Bad   Gateway</html>'), 'Request failed (HTTP 502): <html>Bad Gateway</html>');
  assert.strictEqual(lore.errorMessage(500, ''), 'Request failed (HTTP 500)');
  assert.strictEqual(lore.errorMessage(500, '{"other":1}'), 'Request failed (HTTP 500): {"other":1}');
});

test('deepAvailable needs "deep" in modes', () => {
  assert.strictEqual(lore.deepAvailable({ modes: ['fast', 'deep'] }), true);
  assert.strictEqual(lore.deepAvailable({ modes: ['fast'] }), false);
  assert.strictEqual(lore.deepAvailable({}), false);
  assert.strictEqual(lore.deepAvailable(null), false);
});

test('Enter submits, Shift+Enter and IME composition do not', () => {
  assert.strictEqual(lore.shouldSubmit({ key: 'Enter', shiftKey: false, isComposing: false, keyCode: 13 }), true);
  assert.strictEqual(lore.shouldSubmit({ key: 'Enter', shiftKey: true, isComposing: false, keyCode: 13 }), false);
  assert.strictEqual(lore.shouldSubmit({ key: 'Enter', shiftKey: false, isComposing: true, keyCode: 13 }), false);
  assert.strictEqual(lore.shouldSubmit({ key: 'Enter', shiftKey: false, isComposing: false, keyCode: 229 }), false);
  assert.strictEqual(lore.shouldSubmit({ key: 'a', shiftKey: false, isComposing: false, keyCode: 65 }), false);
});

test('footerText has model, latency and token counts', () => {
  const out = lore.footerText({ model: 'fake', latency_ms: 12, usage: { input_tokens: 3, output_tokens: 4 } });
  assert.strictEqual(out, 'fake · 12 ms · 3 in / 4 out');
});

test('footerText counts cache reads and writes in the input', () => {
  const usage = { input_tokens: 4, output_tokens: 381, cache_read_input_tokens: 9000, cache_creation_input_tokens: 1200 };
  assert.strictEqual(lore.footerText({ model: 'm', latency_ms: 5, usage }), 'm · 5 ms · 10204 in (9000 cached) / 381 out');
  usage.cache_read_input_tokens = 0;
  assert.strictEqual(lore.footerText({ model: 'm', latency_ms: 5, usage }), 'm · 5 ms · 1204 in / 381 out');
});

test('footerText in sources mode says no model', () => {
  const out = lore.footerText({ mode: 'sources', model: 'none', latency_ms: 7, usage: { input_tokens: 0, output_tokens: 0 } });
  assert.strictEqual(out, 'no model · 7 ms');
});

if (!process.exitCode) console.log('web_pure: ' + count + ' checks passed');
