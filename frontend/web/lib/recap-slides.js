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

function renderFeature(s) {
  const items = (s.highlights || [])
    .map((h) => `<div class="recap-hl-item"><em class="recap-hl-arrow">-&gt;</em><span>${esc(h)}</span></div>`)
    .join('');
  const tags = (s.tags || [])
    .map((t) => `<span class="recap-tag">${esc(t)}</span>`)
    .join('');
  const text = `
    ${tags ? `<div class="recap-tags">${tags}</div>` : ''}
    ${s.label ? `<p class="recap-label">${esc(s.label)}</p>` : ''}
    <h2>${esc(s.title || '')}</h2>
    ${s.description ? `<p class="recap-desc">${esc(s.description)}</p>` : ''}
    ${items ? `<div class="recap-highlights">${items}</div>` : ''}
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
function renderSlide(s) {
  switch (s.type) {
    case 'title':     return renderTitle(s);
    case 'section':   return renderSection(s);
    case 'feature':   return renderFeature(s);
    case 'metrics':   return renderMetrics(s);
    case 'bullets':   return renderBullets(s);
    case 'code':      return renderCode(s);
    case 'compare':   return renderCompare(s);
    case 'image':     return renderImage(s);
    case 'coming-up': return renderComingUp(s);
    case 'summary':   return renderSummary(s);
    default:          return renderFeature(s);
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
 * @param {{ title: string, slides: Array<object> }} deck
 * @param {HTMLElement} mountEl
 */
export function renderDeck(deck, mountEl) {
  if (!mountEl) throw new Error('renderDeck: mountEl is required');

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
  slides.forEach((slide, i) => {
    const elDiv = document.createElement('div');
    elDiv.className = `recap-slide recap-slide-${slide.type || 'feature'} recap-no-transition ${i === 0 ? 'pos-current' : 'pos-right'}`;
    elDiv.dataset.index = String(i);
    elDiv.innerHTML = renderSlide(slide);
    deckEl.appendChild(elDiv);
  });

  // Force a reflow so transitions kick in after the initial placement.
  requestAnimationFrame(() => {
    deckEl.querySelectorAll('.recap-slide').forEach((el) => el.classList.remove('recap-no-transition'));
  });

  let current = 0;

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
  deckEl.addEventListener('click', () => go(current + 1));

  function onKeydown(e) {
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
  };

  updateHUD();
}
