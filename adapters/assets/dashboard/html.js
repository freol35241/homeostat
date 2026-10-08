/* Markup for the page: the html tag every module writes it with, and
 * byId for the nodes it lands in. */
export function byId(id) { return document.getElementById(id); }

function esc(s) {
  return String(s).replace(/[&<>"']/g, function (c) {
    return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
  });
}

/* Markup is written with the `html` tag, which escapes by default:
 * html`<b>${name}</b>` escapes every interpolated value unless it is
 * itself markup (another html`` result), and joins an array of either. A
 * forgotten escape is therefore not a way in for a label from the house.
 * All five of & < > " ' are escaped, so a value is safe in text and in a
 * QUOTED attribute alike; an unquoted attribute is not, so every
 * attribute here is quoted. A line break in the template and the
 * indentation after it are dropped, so markup can be laid out over lines
 * without putting whitespace between elements. The result is a Markup,
 * which becomes its text wherever a string is wanted (innerHTML,
 * insertAdjacentHTML) — and is then an ordinary string, so markup is
 * composed by interpolating it, never with `+`. */
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
