// Tests for the dashboard's html tag (adapters/assets/dashboard/html.js):
// escaping is the default, so the page's markup cannot be broken into by
// a label from the house.
'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

// An ES module. Imported through a data: URL, which Node always loads as
// one, whatever a package.json would otherwise say about a .js file.
const source = fs.readFileSync(path.join(__dirname, '../../adapters/assets/dashboard/html.js'), 'utf8');
const loaded = import('data:text/javascript,' + encodeURIComponent(source));

test('every interpolated value is escaped, in text and in a quoted attribute', async () => {
  const { html } = await loaded;
  const label = `<img src=x onerror="alert('hi')"> & more`;
  assert.equal(
    String(html`<span title="${label}">${label}</span>`),
    '<span title="&lt;img src=x onerror=&quot;alert(&#39;hi&#39;)&quot;&gt; &amp; more">' +
      '&lt;img src=x onerror=&quot;alert(&#39;hi&#39;)&quot;&gt; &amp; more</span>'
  );
});

test('markup the tag built is not escaped again', async () => {
  const { html } = await loaded;
  const inner = html`<b>${'a & b'}</b>`;
  assert.equal(String(html`<p>${inner}</p>`), '<p><b>a &amp; b</b></p>');
});

test('a string that looks like markup is still a string', async () => {
  const { html } = await loaded;
  assert.equal(String(html`<p>${'<b>bold</b>'}</p>`), '<p>&lt;b&gt;bold&lt;/b&gt;</p>');
  // Concatenating markup makes a string, and a string is escaped.
  assert.equal(String(html`${html`<i></i>` + ''}`), '&lt;i&gt;&lt;/i&gt;');
});

test('an array is joined, each member escaped or kept as markup', async () => {
  const { html } = await loaded;
  const rows = ['a<', html`<br>`, ['b&', html`<hr>`]];
  assert.equal(String(html`<div>${rows}</div>`), '<div>a&lt;<br>b&amp;<hr></div>');
  assert.equal(String(html`<div>${[]}</div>`), '<div></div>');
});

test('any other value is its string', async () => {
  const { html } = await loaded;
  assert.equal(String(html`${0}|${1.5}|${true}|${false}|${null}|${undefined}`), '0|1.5|true|false|null|undefined');
});

test('a line break and the indentation after it are dropped; other spaces are kept', async () => {
  const { html } = await loaded;
  const out = html`<div class="a">
      <span> x </span>
    </div>`;
  assert.equal(String(out), '<div class="a"><span> x </span></div>');
  // An interpolated value keeps its line breaks: they are text.
  assert.equal(String(html`<pre>${'a\n  b'}</pre>`), '<pre>a\n  b</pre>');
});
