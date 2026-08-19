// Tiny DOM + formatting helpers. Keeps app.js declarative without pulling in a
// framework - the whole point of the plain-HTML approach.

/** Create an element: el('div', {class:'card'}, [child, 'text']). */
export function el(tag, attrs = {}, children = []) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null) continue;
    if (k === 'class') node.className = v;
    else if (k === 'html') node.innerHTML = v;
    else if (k.startsWith('on') && typeof v === 'function') {
      node.addEventListener(k.slice(2).toLowerCase(), v);
    } else if (typeof v === 'boolean') {
      // Boolean attributes (selected/disabled/checked/readonly…): presence = true, so
      // setAttribute(k, false) would STILL mark them on (the value is ignored). Set the
      // property instead, which honours false. This is why a <select>'s options must not
      // all end up `selected` - the last one would win and desync from the saved value.
      node[k] = v;
    } else node.setAttribute(k, v);
  }
  for (const c of [].concat(children)) {
    if (c == null) continue;
    node.appendChild(typeof c === 'string' ? document.createTextNode(c) : c);
  }
  return node;
}

/** Replace all children of a node. */
export function clear(node) {
  node.replaceChildren();
  return node;
}

/** Escape text for safe innerHTML interpolation. */
export function esc(s) {
  return String(s ?? '').replace(/[&<>"']/g, (c) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[c]));
}

/** Relative "time ago" from an ISO string, e.g. "3d ago". '' for null. */
export function ago(iso) {
  if (!iso) return '';
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return '';
  const secs = Math.max(0, (Date.now() - then) / 1000);
  const units = [
    ['y', 31536000], ['mo', 2592000], ['d', 86400],
    ['h', 3600], ['m', 60], ['s', 1],
  ];
  for (const [label, size] of units) {
    if (secs >= size) return `${Math.floor(secs / size)}${label} ago`;
  }
  return 'just now';
}

/** Short absolute date, e.g. "30 Jul 2026". '' for null. */
export function shortDate(iso) {
  if (!iso) return '';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '';
  return d.toLocaleDateString(undefined, { day: '2-digit', month: 'short', year: 'numeric' });
}

let toastTimer = null;
/** Flash a transient message. `isError` styles it red. */
export function toast(message, isError = false) {
  const node = document.getElementById('toast');
  if (!node) return;
  node.textContent = message;
  node.className = 'toast' + (isError ? ' err' : '');
  node.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { node.hidden = true; }, 3200);
}
