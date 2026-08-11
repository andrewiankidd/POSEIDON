// Dependency-free SVG charts. The brief suggested chart.js, but a small
// self-contained SVG renderer honours POSEIDEN's portable-first principle
// better (no ~200 KB vendored blob, works offline / air-gapped) and covers the
// two shapes the Reports view needs: a horizontal bar chart and a success-rate
// gauge. Swapping in chart.js later is a drop-in if richer interactivity is
// wanted - the report DTOs already carry everything it would need.

import { el } from './dom.js';

const SVG = 'http://www.w3.org/2000/svg';
function svgEl(tag, attrs = {}) {
  const node = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
  return node;
}

/**
 * Horizontal bar chart. `data` is `[{label, value}]`. Bars scale to the max
 * value; labels sit left, values right. Returns an SVG element.
 */
export function barChart(data, { width = 520, barHeight = 26, gap = 8 } = {}) {
  if (!data || data.length === 0) return el('div', { class: 'empty' }, 'No data in range.');

  const max = Math.max(...data.map((d) => d.value), 1);
  const labelW = 150;
  const valueW = 44;
  const trackW = width - labelW - valueW;
  const height = data.length * (barHeight + gap);

  const svg = svgEl('svg', {
    class: 'chart',
    viewBox: `0 0 ${width} ${height}`,
    width: '100%',
    height,
    role: 'img',
  });

  data.forEach((d, i) => {
    const y = i * (barHeight + gap);
    const w = Math.max(2, Math.round((d.value / max) * trackW));

    svg.appendChild(svgEl('text', {
      class: 'bar-label', x: labelW - 10, y: y + barHeight / 2 + 4, 'text-anchor': 'end',
    })).textContent = truncate(d.label, 22);

    // Track + bar.
    svg.appendChild(svgEl('rect', {
      x: labelW, y, width: trackW, height: barHeight, rx: 4,
      fill: 'var(--panel-2)',
    }));
    svg.appendChild(svgEl('rect', {
      class: 'bar', x: labelW, y, width: w, height: barHeight, rx: 4,
      fill: 'var(--accent)',
    }));

    svg.appendChild(svgEl('text', {
      class: 'bar-value', x: labelW + trackW + valueW - 4, y: y + barHeight / 2 + 4,
      'text-anchor': 'end',
    })).textContent = String(d.value);
  });

  return svg;
}

/**
 * Success-rate gauge - a donut arc. `rate` is 0..1 or null. Colour shifts with
 * the value (green high, amber mid, red low). Returns an SVG element.
 */
export function gauge(rate, { size = 160 } = {}) {
  const r = size / 2 - 14;
  const cx = size / 2;
  const cy = size / 2;
  const circ = 2 * Math.PI * r;
  const pct = rate == null ? 0 : Math.max(0, Math.min(1, rate));
  const colour = rate == null ? 'var(--ink-soft)'
    : pct >= 0.85 ? 'var(--ok)' : pct >= 0.6 ? 'var(--warn)' : 'var(--err)';

  const svg = svgEl('svg', { viewBox: `0 0 ${size} ${size}`, width: size, height: size });
  svg.appendChild(svgEl('circle', {
    cx, cy, r, fill: 'none', stroke: 'var(--panel-2)', 'stroke-width': 14,
  }));
  const arc = svgEl('circle', {
    cx, cy, r, fill: 'none', stroke: colour, 'stroke-width': 14, 'stroke-linecap': 'round',
    'stroke-dasharray': `${circ * pct} ${circ}`,
    transform: `rotate(-90 ${cx} ${cy})`,
  });
  svg.appendChild(arc);

  const label = svgEl('text', {
    x: cx, y: cy + 6, 'text-anchor': 'middle', 'font-size': 26, 'font-weight': 700,
    fill: 'var(--ink)',
  });
  label.textContent = rate == null ? 'n/a' : `${Math.round(pct * 100)}%`;
  svg.appendChild(label);
  return svg;
}

// Palette for categorical slices/series, cycled by index.
const PALETTE = ['var(--accent)', 'var(--ok)', 'var(--warn)', 'var(--err)', 'var(--run)', 'var(--ink-soft)'];

/**
 * Donut pie chart. `data` is `[{label, value}]`. Slices are proportional; a
 * legend lists label + value. Returns a flex container (chart + legend).
 */
export function pieChart(data, { size = 200 } = {}) {
  const rows = (data || []).filter((d) => d.value > 0);
  if (!rows.length) return el('div', { class: 'empty' }, 'No data in range.');
  const total = rows.reduce((s, d) => s + d.value, 0);
  const r = size / 2 - 4;
  const cx = size / 2;
  const cy = size / 2;
  const svg = svgEl('svg', { viewBox: `0 0 ${size} ${size}`, width: size, height: size, role: 'img' });
  let angle = -Math.PI / 2;
  rows.forEach((d, i) => {
    const frac = d.value / total;
    const end = angle + frac * 2 * Math.PI;
    const large = frac > 0.5 ? 1 : 0;
    const x1 = cx + r * Math.cos(angle), y1 = cy + r * Math.sin(angle);
    const x2 = cx + r * Math.cos(end), y2 = cy + r * Math.sin(end);
    // A single full-circle slice can't be drawn as an arc path; use a circle.
    const path = frac >= 0.999
      ? svgEl('circle', { cx, cy, r, fill: PALETTE[i % PALETTE.length] })
      : svgEl('path', {
          d: `M ${cx} ${cy} L ${x1} ${y1} A ${r} ${r} 0 ${large} 1 ${x2} ${y2} Z`,
          fill: PALETTE[i % PALETTE.length],
        });
    svg.appendChild(path);
    angle = end;
  });
  // Donut hole.
  svg.appendChild(svgEl('circle', { cx, cy, r: r * 0.55, fill: 'var(--panel)' }));

  const legend = el('div', { class: 'chart-legend' }, rows.map((d, i) =>
    el('div', { class: 'legend-row' }, [
      el('span', { class: 'legend-dot', style: `background:${PALETTE[i % PALETTE.length]}` }),
      el('span', { class: 'legend-label' }, truncate(d.label, 24)),
      el('span', { class: 'legend-value' }, String(d.value)),
    ])));
  return el('div', { class: 'chart-pie-wrap' }, [svg, legend]);
}

/**
 * Line chart over ordered points. `series` is `[{label, points:[{label,value}]}]`
 * (one or more overlaid lines). `percent` scales the y-axis to 0-100%.
 */
export function lineChart(series, { width = 560, height = 220, percent = false } = {}) {
  const lines = (series || []).filter((s) => s.points && s.points.length);
  if (!lines.length) return el('div', { class: 'empty' }, 'No data in range.');
  const xs = lines[0].points.map((p) => p.label);
  const maxV = percent ? 1 : Math.max(1, ...lines.flatMap((s) => s.points.map((p) => p.value)));
  const padL = 44, padB = 24, padT = 10, padR = 10;
  const plotW = width - padL - padR, plotH = height - padT - padB;
  const xAt = (i) => padL + (xs.length <= 1 ? plotW / 2 : (i / (xs.length - 1)) * plotW);
  const yAt = (v) => padT + plotH - (v / maxV) * plotH;

  const svg = svgEl('svg', { class: 'chart', viewBox: `0 0 ${width} ${height}`, width: '100%', height, role: 'img' });
  // Axes.
  svg.appendChild(svgEl('line', { x1: padL, y1: padT, x2: padL, y2: padT + plotH, stroke: 'var(--border)' }));
  svg.appendChild(svgEl('line', { x1: padL, y1: padT + plotH, x2: padL + plotW, y2: padT + plotH, stroke: 'var(--border)' }));
  // Y label (max).
  const top = svgEl('text', { x: padL - 6, y: padT + 8, 'text-anchor': 'end', class: 'axis-label' });
  top.textContent = percent ? '100%' : String(maxV);
  svg.appendChild(top);
  // X endpoints.
  [0, xs.length - 1].forEach((i) => {
    if (i < 0) return;
    const t = svgEl('text', { x: xAt(i), y: height - 6, 'text-anchor': 'middle', class: 'axis-label' });
    t.textContent = truncate(xs[i], 10);
    svg.appendChild(t);
  });
  lines.forEach((s, li) => {
    const colour = PALETTE[li % PALETTE.length];
    const d = s.points.map((p, i) => `${i === 0 ? 'M' : 'L'} ${xAt(i)} ${yAt(p.value)}`).join(' ');
    svg.appendChild(svgEl('path', { d, fill: 'none', stroke: colour, 'stroke-width': 2 }));
    s.points.forEach((p, i) => svg.appendChild(svgEl('circle', { cx: xAt(i), cy: yAt(p.value), r: 3, fill: colour })));
  });
  return svg;
}

function truncate(s, n) {
  s = String(s ?? '');
  return s.length > n ? s.slice(0, n - 1) + '…' : s;
}
