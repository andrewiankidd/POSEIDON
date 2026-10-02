// Recap slide renderer - a self-contained, dependency-free port of OCTOGON's
// slide deck. POSEIDON's frontend is plain HTML/JS with no build step, so this
// module has no imports and no external packages: it consumes a plain JS deck
// object (not YAML) and renders an interactive, keyboard-navigable slide deck.
//
// Deck shape: { title: string, slides: Array<Slide> }. Each slide is a plain
// object with a `type` field (see SLIDE_TYPES) plus per-type fields - the field
// shapes match OCTOGON exactly, so decks are compatible across the two tools.
//
// Styling lives in styles.css under the "/* Recap slides */" section; all
// classes are prefixed `recap-` to avoid collisions with the rest of the app.

/** The slide types this renderer understands. */
export const SLIDE_TYPES = [
  'title',
  'section',
  'feature',
  'metrics',
  'bullets',
  'code',
  'compare',
  'image',
  'coming-up',
  'summary',
];

/** Escape text for safe innerHTML interpolation. */
function esc(str) {
  return String(str ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

// ── Slide renderers ──────────────────────────────────────────────────────────
// Each takes a slide object and returns an HTML string. Ported one-for-one from
// OCTOGON's src/slides.js, with `recap-` class prefixes.

function renderTitle(s) {
  const meta = (s.meta || []).map((m) => `<span class="recap-meta-item">${esc(m)}</span>`).join('');
  return `
    ${s.eyebrow ? `<p class="recap-eyebrow">${esc(s.eyebrow)}</p>` : ''}
    <h1>${esc(s.title || '')}</h1>
    ${s.subtitle ? `<p class="recap-subtitle">${esc(s.subtitle)}</p>` : ''}
    ${meta ? `<div class="recap-meta-row">${meta}</div>` : ''}
  `;
}

function renderSection(s) {
  return `
    ${s.number ? `<div class="recap-sec-num">${esc(s.number)}</div>` : ''}
    <h2>${esc(s.title || '')}</h2>
    ${s.subtitle ? `<p class="recap-sec-sub">${esc(s.subtitle)}</p>` : ''}
  `;
}

// `editable` (the live preview only - never the exported file) adds a remove button to
// every grouped row and makes the summary click-to-edit; renderDeck wires the events.
function renderFeature(s, editable = false) {
  let highlightHtml = '';
  if (s.groups && s.groups.length) {
    highlightHtml = '<div class="recap-highlights">' + s.groups.map((g, gi) => {
      const rows = (g.items || [])
        .map((h, ii) => `<div class="recap-hl-item"><em class="recap-hl-arrow">-&gt;</em><span>${esc(h)}</span>${
          editable
            ? `<button type="button" class="recap-hl-remove" data-remove data-g="${gi}" data-i="${ii}" title="Remove this item from the deck" aria-label="Remove item">×</button>`
            : ''}</div>`)
        .join('');
      return (g.heading ? `<div class="recap-hl-group-heading">${esc(g.heading)}</div>` : '') + rows;
    }).join('') + '</div>';
  } else {
    const rows = (s.highlights || [])
      .map((h) => `<div class="recap-hl-item"><em class="recap-hl-arrow">-&gt;</em><span>${esc(h)}</span></div>`)
      .join('');
    if (rows) highlightHtml = `<div class="recap-highlights">${rows}</div>`;
  }
  const tags = (s.tags || [])
    .map((t) => `<span class="recap-tag">${esc(t)}</span>`)
    .join('');
  const text = `
    ${tags ? `<div class="recap-tags">${tags}</div>` : ''}
    ${s.label ? `<p class="recap-label">${esc(s.label)}</p>` : ''}
    <h2>${esc(s.title || '')}</h2>
    ${editable
      ? `<p class="recap-desc recap-editable" contenteditable="plaintext-only" spellcheck="true" data-edit="description" title="Click to edit this summary">${esc(s.description || '')}</p>`
      : s.description ? `<p class="recap-desc">${esc(s.description)}</p>` : ''}
    ${highlightHtml}
  `;
  if (s.image) {
    return `
      <div class="recap-feature-split">
        <div class="recap-feature-text">${text}</div>
        <div class="recap-feature-img-wrap">
          <img src="${esc(s.image)}" alt="${esc(s.title || '')}" />
        </div>
      </div>
    `;
  }
  return text;
}

function renderMetrics(s) {
  const cards = (s.metrics || []).map((m) => {
    const colorClass = m.color ? `recap-c-${esc(m.color)}` : '';
    return `
      <div class="recap-metric-card">
        <div class="recap-metric-val ${colorClass}">${esc(m.value)}</div>
        <div class="recap-metric-label">${esc(m.label)}</div>
        ${m.sub ? `<div class="recap-metric-sub">${esc(m.sub)}</div>` : ''}
      </div>
    `;
  }).join('');
  return `
    ${s.title ? `<h2>${esc(s.title)}</h2>` : ''}
    <div class="recap-metrics-grid">${cards}</div>
  `;
}

function renderBullets(s) {
  const items = (s.items || [])
    .map((item) => `<div class="recap-bullet-item"><div class="recap-bullet-dot"></div><span>${esc(item)}</span></div>`)
    .join('');
  return `
    ${s.title ? `<h2>${esc(s.title)}</h2>` : ''}
    <div class="recap-bullets-list">${items}</div>
  `;
}

function renderCode(s) {
  // Dependency-free: no syntax highlighter (OCTOGON used highlight.js). The code
  // is escaped and shown verbatim; the language label is kept for context.
  const lang = s.language || 'yaml';
  return `
    ${s.title ? `<h2>${esc(s.title)}</h2>` : ''}
    <div class="recap-code-wrap">
      <div class="recap-code-titlebar">
        <span class="recap-wb recap-wb-r"></span>
        <span class="recap-wb recap-wb-y"></span>
        <span class="recap-wb recap-wb-g"></span>
        <span class="recap-code-lang">${esc(lang)}</span>
      </div>
      <pre><code class="recap-code-block language-${esc(lang)}">${esc(s.code || '')}</code></pre>
    </div>
  `;
}

function renderImage(s) {
  return `
    ${s.title ? `<p class="recap-img-title">${esc(s.title)}</p>` : ''}
    <div class="recap-img-wrap">
      <img src="${esc(s.src)}" alt="${esc(s.alt || s.title || '')}" />
    </div>
    ${s.caption ? `<p class="recap-img-caption">${esc(s.caption)}</p>` : ''}
  `;
}

function renderCompare(s) {
  function col(data, isAfter) {
    const items = (data.items || []).map((item) => {
      const type = item.type || 'neutral';
      const icon = type === 'good' ? '✓' : type === 'bad' ? '✕' : '·';
      return `
        <div class="recap-compare-item">
          <span class="recap-ci-icon ${esc(type)}">${icon}</span>
          <span>${esc(item.text)}</span>
        </div>
      `;
    }).join('');
    return `
      <div class="recap-compare-col ${isAfter ? 'is-after' : ''}">
        <div class="recap-compare-col-label">${esc(data.label || (isAfter ? 'After' : 'Before'))}</div>
        <div class="recap-compare-items">${items}</div>
      </div>
    `;
  }
  return `
    ${s.title ? `<h2>${esc(s.title)}</h2>` : ''}
    <div class="recap-compare-grid">
      ${col(s.before || {}, false)}
      ${col(s.after || {}, true)}
    </div>
  `;
}

function renderComingUp(s) {
  const items = (s.items || [])
    .map((item) => `
      <div class="recap-cu-item">
        <span class="recap-cu-arrow">-&gt;</span>
        <span>${esc(item)}</span>
      </div>`)
    .join('');
  return `
    ${s.eyebrow ? `<p class="recap-cu-eyebrow">${esc(s.eyebrow)}</p>` : ''}
    <h2>${esc(s.title || 'Coming up')}</h2>
    <div class="recap-cu-items">${items}</div>
  `;
}

function renderSummary(s) {
  const stats = (s.stats || []).map((m) => {
    const cc = m.color ? `recap-c-${esc(m.color)}` : '';
    return `
      <div class="recap-sm-stat">
        <span class="recap-sm-stat-val ${cc}">${esc(m.value)}</span>
        <span class="recap-sm-stat-label">${esc(m.label)}</span>
      </div>`;
  }).join('');

  const shipped = (s.shipped || []).map((group) => {
    const items = (group.items || [])
      .map((item) => `<div class="recap-sm-item">${esc(item)}</div>`)
      .join('');
    return `
      <div class="recap-sm-group">
        <div class="recap-sm-group-title">${esc(group.area)}</div>
        <div class="recap-sm-group-items">${items}</div>
      </div>`;
  }).join('');

  const upcoming = (s.upcoming || [])
    .map((item) => `<div class="recap-sm-upcoming-item">${esc(item)}</div>`)
    .join('');

  return `
    <div class="recap-sm-top">
      <div class="recap-sm-heading">${esc(s.title || 'At a glance')}</div>
      ${stats ? `<div class="recap-sm-stats">${stats}</div>` : ''}
    </div>
    <div class="recap-sm-body">
      <div class="recap-sm-shipped">${shipped}</div>
    </div>
    ${upcoming ? `
      <div class="recap-sm-upcoming">
        <div class="recap-sm-upcoming-label">Next up</div>
        <div class="recap-sm-upcoming-items">${upcoming}</div>
      </div>` : ''}
  `;
}

/** Render a single slide object to an HTML string. */
function renderSlide(s, editable = false) {
  switch (s.type) {
    case 'title':     return renderTitle(s);
    case 'section':   return renderSection(s);
    case 'feature':   return renderFeature(s, editable);
    case 'metrics':   return renderMetrics(s);
    case 'bullets':   return renderBullets(s);
    case 'code':      return renderCode(s);
    case 'compare':   return renderCompare(s);
    case 'image':     return renderImage(s);
    case 'coming-up': return renderComingUp(s);
    case 'summary':   return renderSummary(s);
    default:          return renderFeature(s, editable);
  }
}

// ── Deck ─────────────────────────────────────────────────────────────────────

/**
 * Render an interactive slide deck into `mountEl`. Shows one slide at a time
 * with the same navigation OCTOGON has: click / Space / ArrowRight (also
 * ArrowDown / PageDown) advance, ArrowLeft (also ArrowUp / PageUp) go back,
 * Home jumps to the first slide, End to the last. A counter tracks position.
 *
 * Calling it again on the same `mountEl` tears down the previous instance's
 * keydown listener first, so re-rendering never leaks handlers.
 *
 * `opts.startIndex` opens on that slide instead of the first, so a caller can re-render
 * a patched deck (e.g. once AI summaries land) without bouncing the viewer back to
 * slide 1; `mountEl._recapIndex()` reports the current slide for that purpose.
 *
 * `opts.editable` turns the preview into a WYSIWYG editor: each grouped item gets a
 * remove button and the summary becomes click-to-edit. Edits mutate `deck` itself (so
 * the exported file carries them) and are reported through `opts.onEdit(slide, kind)`
 * with kind `'remove'` or `'description'`. The exported standalone deck never passes it.
 * `opts.scrollTop` restores the current slide's scroll position after a re-render.
 *
 * @param {{ title: string, slides: Array<object> }} deck
 * @param {HTMLElement} mountEl
 * @param {{ startIndex?: number, editable?: boolean, onEdit?: (slide: object, kind: string) => void, scrollTop?: number }} [opts]
 */
export function renderDeck(deck, mountEl, opts = {}) {
  if (!mountEl) throw new Error('renderDeck: mountEl is required');
  const editable = !!opts.editable;

  // Clean up a previous render on this mount (removes its keydown listener).
  if (typeof mountEl._recapCleanup === 'function') mountEl._recapCleanup();

  const slides = (deck && deck.slides) || [];
  const title = (deck && deck.title) || '';
  const total = slides.length;

  mountEl.innerHTML = `
    <div class="recap-root">
      <div class="recap-progress" data-recap="progress" style="width:0"></div>
      <div class="recap-deck" data-recap="deck"></div>
      <div class="recap-nav">
        <span class="recap-nav-title">${esc(title)}</span>
        <span class="recap-nav-counter"><span data-recap="num">${total ? 1 : 0}</span> / ${total}</span>
        <div class="recap-nav-btns">
          <button class="recap-nav-btn" data-recap="prev">&larr; Prev</button>
          <button class="recap-nav-btn" data-recap="next">Next &rarr;</button>
        </div>
      </div>
    </div>
  `;

  const root = mountEl.querySelector('.recap-root');
  const deckEl = root.querySelector('[data-recap="deck"]');
  const numEl = root.querySelector('[data-recap="num"]');
  const progressEl = root.querySelector('[data-recap="progress"]');
  const prevBtn = root.querySelector('[data-recap="prev"]');
  const nextBtn = root.querySelector('[data-recap="next"]');

  // Render every slide into the DOM up front; position them off-screen and let
  // CSS transitions reveal the current one (OCTOGON's approach).
  const start = Math.max(0, Math.min(total - 1, opts.startIndex || 0));
  slides.forEach((slide, i) => {
    const elDiv = document.createElement('div');
    const pos = i === start ? 'pos-current' : i < start ? 'pos-left' : 'pos-right';
    elDiv.className = `recap-slide recap-slide-${slide.type || 'feature'} recap-no-transition ${pos}`;
    elDiv.dataset.index = String(i);
    elDiv.innerHTML = renderSlide(slide, editable);
    deckEl.appendChild(elDiv);
  });
  const shown = deckEl.querySelector('.pos-current');
  if (shown && opts.scrollTop) shown.scrollTop = opts.scrollTop;
  setDeckLogo(mountEl, deck && deck.logo, { position: deck && deck.logoPosition, opacity: deck && deck.logoOpacity });

  // Force a reflow so transitions kick in after the initial placement.
  requestAnimationFrame(() => {
    deckEl.querySelectorAll('.recap-slide').forEach((el) => el.classList.remove('recap-no-transition'));
  });

  let current = start;
  mountEl._recapIndex = () => current;

  function go(n) {
    const prev = current;
    current = Math.max(0, Math.min(total - 1, n));
    if (prev === current) return;

    deckEl.querySelectorAll('.recap-slide').forEach((el, i) => {
      el.classList.remove('pos-current', 'pos-left', 'pos-right');
      if (i === current) el.classList.add('pos-current');
      else if (i < current) el.classList.add('pos-left');
      else el.classList.add('pos-right');
    });

    updateHUD();
  }

  function updateHUD() {
    if (numEl) numEl.textContent = String(total ? current + 1 : 0);
    if (progressEl) progressEl.style.width = total ? `${((current + 1) / total) * 100}%` : '0';
    if (prevBtn) prevBtn.disabled = current === 0;
    if (nextBtn) nextBtn.disabled = current >= total - 1;
  }

  prevBtn.addEventListener('click', (e) => { e.stopPropagation(); go(current - 1); });
  nextBtn.addEventListener('click', (e) => { e.stopPropagation(); go(current + 1); });
  deckEl.addEventListener('click', (e) => {
    if (editable) {
      const remove = e.target.closest('[data-remove]');
      if (remove) { e.stopPropagation(); removeItem(remove); return; }
      if (e.target.closest('[data-edit]')) return; // editing the summary, not paging
    }
    go(current + 1);
  });

  // Drop one listed item (and its group if that empties it), then re-render in place
  // at the same slide + scroll offset so removing a run of rows doesn't jump around.
  function removeItem(btn) {
    const slideEl = btn.closest('.recap-slide');
    const slide = slides[Number(slideEl.dataset.index)];
    const gi = Number(btn.dataset.g);
    const group = slide && slide.groups && slide.groups[gi];
    if (!group) return;
    group.items.splice(Number(btn.dataset.i), 1);
    if (!group.items.length) slide.groups.splice(gi, 1);
    if (opts.onEdit) opts.onEdit(slide, 'remove');
    renderDeck(deck, mountEl, { ...opts, startIndex: current, scrollTop: slideEl.scrollTop });
  }

  // Commit the summary when the user leaves the field (plain text only).
  deckEl.addEventListener('focusout', (e) => {
    const field = editable && e.target.closest && e.target.closest('[data-edit="description"]');
    if (!field) return;
    const slide = slides[Number(field.closest('.recap-slide').dataset.index)];
    const text = field.innerText.replace(/ /g, ' ').trim();
    if (slide && text !== (slide.description || '')) {
      slide.description = text;
      if (opts.onEdit) opts.onEdit(slide, 'description');
    }
  });

  function onKeydown(e) {
    // Typing in the summary must not page the deck (Space/arrows are text editing keys).
    const typing = e.target && e.target.closest && e.target.closest('[contenteditable], input, textarea, select');
    if (typing) {
      if (e.key === 'Escape' && typing.blur) typing.blur();
      return;
    }
    switch (e.key) {
      case 'ArrowRight': case 'ArrowDown': case ' ': case 'PageDown':
        e.preventDefault(); go(current + 1); break;
      case 'ArrowLeft': case 'ArrowUp': case 'PageUp':
        e.preventDefault(); go(current - 1); break;
      case 'Home': e.preventDefault(); go(0); break;
      case 'End': e.preventDefault(); go(total - 1); break;
      default: break;
    }
  }
  document.addEventListener('keydown', onKeydown);

  // Expose teardown so a later renderDeck on this mount can unhook cleanly.
  mountEl._recapCleanup = () => {
    document.removeEventListener('keydown', onKeydown);
    mountEl._recapCleanup = null;
    mountEl._recapIndex = null;
  };

  updateHUD();
}

// ── Branding logo ────────────────────────────────────────────────────────────
// `deck.logo` is a base64 image data URL drawn faintly bottom-right of every slide. It is
// only ever a data URL (so the exported file stays self-contained) and is validated here
// as well as on the server, because a deck can come from an imported config.

const LOGO_RE = /^data:image\/(png|jpeg|gif|webp|svg\+xml);base64,[A-Za-z0-9+/=]+$/;

/** The corners the logo can sit in (value is also the stored tenant setting). */
export const LOGO_POSITIONS = [
  { value: 'top-right', label: 'Top right' },
  { value: 'top-left', label: 'Top left' },
  { value: 'bottom-right', label: 'Bottom right' },
  { value: 'bottom-left', label: 'Bottom left' },
];
export const DEFAULT_LOGO_POSITION = 'top-right';
export const DEFAULT_LOGO_OPACITY = 40; // percent
export const LOGO_OPACITY_RANGE = { min: 5, max: 100 };

/** A valid corner value, else the default. */
export function normalizeLogoPosition(v) {
  const p = String(v ?? '').trim().toLowerCase();
  return LOGO_POSITIONS.some((o) => o.value === p) ? p : DEFAULT_LOGO_POSITION;
}

/** Opacity percent clamped to the allowed range (default when not a number). */
export function normalizeLogoOpacity(v) {
  const n = Math.round(Number(v));
  if (v == null || v === '' || !Number.isFinite(n)) return DEFAULT_LOGO_OPACITY;
  return Math.min(LOGO_OPACITY_RANGE.max, Math.max(LOGO_OPACITY_RANGE.min, n));
}

/** True when `v` is an allowed base64 image data URL. */
export function isLogoDataUrl(v) {
  return typeof v === 'string' && LOGO_RE.test(v);
}

/**
 * Show (or, with a falsy/invalid `url`, remove) the logo overlay on a rendered deck.
 * `opts.position` (a corner) and `opts.opacity` (percent) are validated here, so a deck
 * built from an imported config can't smuggle in anything but those.
 */
export function setDeckLogo(mountEl, url, opts = {}) {
  const deckEl = mountEl && mountEl.querySelector('.recap-deck');
  if (!deckEl) return;
  let img = deckEl.querySelector('.recap-logo');
  if (!isLogoDataUrl(url)) { if (img) img.remove(); return; }
  if (!img) {
    img = document.createElement('img');
    img.className = 'recap-logo';
    img.alt = '';
    img.setAttribute('aria-hidden', 'true');
    deckEl.appendChild(img);
  }
  img.dataset.pos = normalizeLogoPosition(opts.position);
  img.style.opacity = String(normalizeLogoOpacity(opts.opacity) / 100);
  img.src = url;
}

// ── Theme ────────────────────────────────────────────────────────────────────
// The deck is styled entirely from CSS custom properties, so a theme is just a handful
// of overrides. Three base colours drive everything; the surfaces, borders and muted text
// are derived from them with color-mix so a user only picks `bg`, `ink` and `accent`.
// The same function feeds the live view (inline style on the deck host) and the exported
// HTML (a :root block), so what you see is exactly what you download.

/** The user-pickable base colours, in display order. */
export const RECAP_THEME_KEYS = [
  { key: 'bg', label: 'Background', cssVar: '--bg' },
  { key: 'ink', label: 'Text', cssVar: '--ink' },
  { key: 'accent', label: 'Accent', cssVar: '--accent' },
];

/** Normalise `#abc` / `abc` / `#AABBCC` to lowercase `#aabbcc`, or null if not a hex colour. */
export function normalizeHex(v) {
  let s = String(v ?? '').trim().replace(/^#/, '').toLowerCase();
  if (/^[0-9a-f]{3}$/.test(s)) s = s.split('').map((c) => c + c).join('');
  return /^[0-9a-f]{6}$/.test(s) ? `#${s}` : null;
}

/** CSS custom-property overrides for a theme `{ bg?, ink?, accent? }`. Unset keys are left
 *  to the app's own light/dark palette. */
export function recapThemeVars(theme = {}) {
  const bg = normalizeHex(theme.bg);
  const ink = normalizeHex(theme.ink);
  const accent = normalizeHex(theme.accent);
  const vars = {};
  if (bg) vars['--bg'] = bg;
  if (ink) vars['--ink'] = ink;
  if (accent) vars['--accent'] = accent;
  if (bg || ink) {
    // Surfaces step from the background toward the text colour, whichever way round.
    vars['--panel'] = 'color-mix(in srgb, var(--bg) 94%, var(--ink))';
    vars['--panel-2'] = 'color-mix(in srgb, var(--bg) 89%, var(--ink))';
    vars['--border'] = 'color-mix(in srgb, var(--bg) 84%, var(--ink))';
    vars['--ink-soft'] = 'color-mix(in srgb, var(--ink) 62%, var(--bg))';
  }
  return vars;
}

/** The same overrides as a `prop:value;...` string, for the exported HTML's stylesheet. */
export function recapThemeCss(theme) {
  return Object.entries(recapThemeVars(theme)).map(([k, v]) => `${k}:${v}`).join(';');
}
