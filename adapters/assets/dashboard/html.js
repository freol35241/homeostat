/* Markup for the page: the html tag every module writes it with, and
 * byId for the nodes it lands in. */
export function byId(id) { return document.getElementById(id); }

function esc(s) {
  return String(s).replace(/[&<>"']/g, function (c) {
    return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
  });
}

/* Markup is written with the `html` tag, which escapes by default.
 * html`<b>${name}</b>` escapes every interpolated value unless it is
 * itself markup (another html`` result), and joins an array of either.
 * A label from the house therefore cannot inject markup through a
 * forgotten escape.
 * All five of & < > " ' are escaped, so a value is safe in text and in a
 * quoted attribute. It is not safe in an unquoted attribute, so every
 * attribute here is quoted.
 * A line break in the template and the indentation after it are dropped,
 * so markup can be laid out over several lines without adding whitespace
 * between elements.
 * The result is a Markup. It turns into its text wherever a string is
 * expected (innerHTML, insertAdjacentHTML), and is then an ordinary
 * string. Compose markup by interpolating it, not with `+`. */
function Markup(text) { this.text = text; }

Markup.prototype.toString = function () { return this.text; };

export function html(strings) {
  var out = unlaid(strings[0]);
  for (var i = 1; i < strings.length; i++) out += markupOf(arguments[i]) + unlaid(strings[i]);
  return new Markup(out);
}

function unlaid(text) { return text.replace(/\n\s*/g, ''); }

function markupOf(value) {
  if (value instanceof Markup) return value.text;
  if (Array.isArray(value)) return value.map(markupOf).join('');
  return esc(value);
}
