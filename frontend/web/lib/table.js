// Reusable data table: click-to-sort column headers + a per-column filter row,
// with a live "showing X of Y" count and a clear-filters affordance. Framework-
// free, built on the same `el`/`clear` helpers as the rest of the frontend.
//
// A column is:
//   {
//     label,                    // header text
//     value: (row) => primitive // sort key + filter text (string | number)
//     render: (row) => node|str // cell content; defaults to text of value()
//     sort: 'text'|'number'|false // sortable + how to compare (default 'text')
//     filter: boolean           // show a filter input for this column (default true)
//     align: 'right'            // cell + header text-align
//     class: 'wrap'             // extra class on the <td>
//   }
//
// Options: { initialSort:{index,dir}, emptyText, predicate:(row)=>bool,
//            toolbar:[nodes] }. `predicate` is an external filter the caller can
// flip (e.g. "flagged only") and then call the returned node's `.refresh()`.

import { el, clear, toast } from './dom.js';

// Default per-column filter matcher. Comma-separated terms are OR'd; a term
// prefixed with `!` excludes. Both `value` and `query` are already lower-cased.
//   "active, resolved"  → state is active OR resolved
//   "!closed"           → anything except closed
//   "active, !wip"      → active AND not wip
export function matchesFilter(value, query) {
  const pos = [];
  const neg = [];
  for (const raw of query.split(',')) {
    const t = raw.trim();
    if (!t) continue;
    if (t[0] === '!') { const n = t.slice(1).trim(); if (n) neg.push(n); }
    else pos.push(t);
  }
  if (neg.some((n) => value.includes(n))) return false;
  return pos.length === 0 || pos.some((p) => value.includes(p));
}

// Funnel glyph for choice-filter columns (theme-aware via currentColor).
const FUNNEL_SVG =
  '<svg viewBox="0 0 16 16" width="11" height="11" aria-hidden="true">' +
  '<path fill="currentColor" d="M1.4 2h13.2a.5.5 0 0 1 .38.82L10 9.2v4a.5.5 0 0 1-.72.45l-2-1A.5.5 0 0 1 7 12.2V9.2L1.02 2.82A.5.5 0 0 1 1.4 2Z"/></svg>';

export function dataTable(columns, rows, opts = {}) {
  const filters = columns.map(() => '');
  // Per-column checkbox filter (opt-in via col.filterChoices). null = no filter
  // (all values pass); a Set of lower-cased allowed values = show only those.
  const choiceSel = columns.map(() => null);
  const choiceSync = []; // fns that refresh each funnel button's active state
  let sortIndex = opts.initialSort?.index ?? null;
  let sortDir = opts.initialSort?.dir ?? 1; // 1 asc, -1 desc
  let page = 0;
  const pageSize = opts.pageSize > 0 ? opts.pageSize : 0; // 0 = show all

  // Optional cross-refresh persistence: when `opts.persistKey` is set, the view's
  // filters + choice selections + sort are mirrored to localStorage so a reload (or
  // navigating away and back) restores them. Selection + page are intentionally NOT
  // persisted (they're tied to a specific data snapshot). Rehydrated here, BEFORE the
  // header inputs are built, so their initial values reflect the restored state.
  const persistKey = opts.persistKey ? `poseiden.tablefilters.${opts.persistKey}` : null;
  loadState();

  // Multi-select. `selected` holds row keys; selection persists across filtering
  // + sorting (by key), so hidden-but-selected rows stay selected.
  const selectable = !!opts.selectable;
  const keyOf = opts.rowKey || ((row) => row.id ?? row.pipeline_id ?? JSON.stringify(row));
  const selected = new Set();
  let lastShown = rows; // the filtered+sorted set, for select-all + header state
  let headCheckbox = null;

  const arrows = [];
  const headCells = [];
  const headRow = el('tr', { class: 'dt-head' });
  const filterRow = el('tr', { class: 'dt-filter' });

  if (selectable) {
    headCheckbox = el('input', { type: 'checkbox', class: 'dt-check', title: 'Select all', onchange: () => toggleAll() });
    headRow.appendChild(el('th', { class: 'dt-check-cell' }, headCheckbox));
    filterRow.appendChild(el('th', { class: 'dt-check-cell' }));
  }

  columns.forEach((col, i) => {
    const sortable = col.sort !== false;
    const arrow = el('span', { class: 'dt-arrow' }, '');
    arrows.push(arrow);
    const th = el(
      'th',
      sortable ? { class: 'dt-sortable', onclick: () => cycleSort(i) } : {},
      [col.label ? el('span', {}, col.label) : '', sortable ? arrow : null],
    );
    if (col.align) th.style.textAlign = col.align;
    headCells.push(th);
    headRow.appendChild(th);

    const fth = el('th', {});
    if (col.filter !== false) {
      // A column with an enumerable value set (State, Type, …) gets a funnel that
      // opens a checkbox list of its unique values - no need to know the !exclude
      // syntax. Everything else keeps the free-text filter input.
      if (col.filterChoices) fth.appendChild(buildChoiceFilter(i, col));
      else {
        fth.appendChild(el('input', {
          class: 'dt-filter-input', type: 'text', value: filters[i],
          placeholder: col.filterPlaceholder || 'filter', 'aria-label': `Filter ${col.label}`,
          title: col.filterMatch ? undefined : 'comma = match any (active, resolved); ! = exclude (!closed)',
          oninput: (e) => { filters[i] = e.target.value.trim().toLowerCase(); page = 0; saveState(); render(); },
        }));
      }
    }
    filterRow.appendChild(fth);
  });

  const tbody = el('tbody');
  const table = el('table', {}, [el('thead', {}, [headRow, filterRow]), tbody]);
  const tableWrap = el('div', { class: 'table-wrap' }, table);

  const count = el('span', { class: 'dt-count' }, '');
  if (opts.onCountClick) {
    // The count doubles as a shortcut to the pagination setting.
    count.classList.add('dt-count-link');
    count.title = 'Change rows per page';
    count.addEventListener('click', opts.onCountClick);
  }
  const clearLink = el('a', {
    class: 'dt-clear', href: '#', onclick: (e) => { e.preventDefault(); resetFilters(); },
  }, 'Clear');
  // "N selected" + a clear-selection link, shown above the table when selecting.
  const selCount = el('span', { class: 'dt-selected' }, '');
  const selClear = el('a', {
    class: 'dt-sel-clear', href: '#', onclick: (e) => { e.preventDefault(); selected.clear(); render(); },
  }, 'clear');
  const toolbar = el('div', { class: 'dt-toolbar' }, [
    ...(opts.toolbar || []), selCount, selClear, el('span', { class: 'dt-spacer' }), count, clearLink,
  ]);

  // Pager (only shown when there's more than one page).
  const prevBtn = el('button', { class: 'btn btn-ghost dt-page-btn', onclick: () => { if (page > 0) { page--; render(); } } }, '‹ Prev');
  const nextBtn = el('button', { class: 'btn btn-ghost dt-page-btn', onclick: () => { page++; render(); } }, 'Next ›');
  const pageLabel = el('span', { class: 'dt-page-label' }, '');
  const pager = el('div', { class: 'dt-pager' }, [prevBtn, pageLabel, nextBtn]);

  const wrap = el('div', {}, [toolbar, tableWrap, pager]);
  // Fill mode: the wrap becomes a flex column so only the table scrolls (header +
  // toolbar stay fixed). Keep the sticky filter row's offset in sync with the real
  // header-row height, which varies with zoom / font.
  if (opts.fill) {
    wrap.classList.add('dt-fill');
    if (typeof ResizeObserver === 'function') {
      const ro = new ResizeObserver(() => {
        table.style.setProperty('--dt-head-h', `${headRow.offsetHeight}px`);
      });
      ro.observe(headRow);
    }
  }

  function valueOf(col, row) {
    return col.value ? col.value(row) : '';
  }

  // The choice value(s) of a row for a column. Multi-value columns (e.g. Tags)
  // supply `col.choiceValues: (row) => [...]`; single-value columns fall back to the
  // sort/filter `value`. An empty result collapses to [''] so blank/untagged rows are
  // still representable + filterable.
  function choiceValuesOf(col, row) {
    const raw = col.choiceValues ? col.choiceValues(row) : [valueOf(col, row)];
    const arr = (Array.isArray(raw) ? raw : [raw]).map((v) => String(v ?? '').trim());
    return arr.length ? arr : [''];
  }

  // The distinct values of a choice column, from the CURRENT data (so it tracks
  // setRows). Keyed by lower-case; blanks collapse to one "(blank)" entry.
  function uniqueValues(col) {
    const map = new Map(); // lower -> display
    for (const row of rows) {
      for (const raw of choiceValuesOf(col, row)) {
        const low = raw.toLowerCase();
        if (!map.has(low)) map.set(low, raw === '' ? '(blank)' : raw);
      }
    }
    return [...map.entries()]
      .map(([low, display]) => ({ low, display }))
      .sort((a, b) => a.display.localeCompare(b.display));
  }

  // A funnel button that toggles a checkbox dropdown of the column's unique values.
  // The panel is fixed-positioned on <body> so the table-wrap's overflow can't clip
  // it; it closes on outside click, Escape, or scroll.
  function buildChoiceFilter(i, col) {
    const btn = el('button', {
      type: 'button', class: 'dt-filter-btn', title: `Filter ${col.label}`,
      'aria-label': `Filter ${col.label}`, html: FUNNEL_SVG,
    });
    const holder = el('div', { class: 'dt-choice' }, btn);
    let panel = null;
    const close = () => {
      if (!panel) return;
      panel.remove(); panel = null;
      document.removeEventListener('click', onDoc, true);
      document.removeEventListener('keydown', onKey, true);
      window.removeEventListener('scroll', onScroll, true);
      window.removeEventListener('resize', close);
    };
    const onDoc = (e) => {
      if (holder.contains(e.target)) return;         // the funnel itself
      if (panel && panel.contains(e.target)) return; // clicks inside the dropdown
      close();
    };
    // Close only on PAGE/table scroll (the fixed panel would float away) - NOT when
    // scrolling the dropdown's own value list.
    const onScroll = (e) => { if (!panel || !panel.contains(e.target)) close(); };
    const onKey = (e) => { if (e.key === 'Escape') close(); };
    btn.addEventListener('click', (e) => {
      e.stopPropagation();
      if (panel) { close(); return; }
      panel = buildChoicePanel(i, col);
      document.body.appendChild(panel);
      const r = btn.getBoundingClientRect();
      const width = panel.offsetWidth || 200;
      panel.style.top = `${Math.round(r.bottom + 4)}px`;
      panel.style.left = `${Math.round(Math.max(8, Math.min(r.left, window.innerWidth - width - 8)))}px`;
      panel.querySelector('.dt-choice-search')?.focus();
      document.addEventListener('click', onDoc, true);
      document.addEventListener('keydown', onKey, true);
      window.addEventListener('scroll', onScroll, true);
      window.addEventListener('resize', close);
    });
    const sync = () => btn.classList.toggle('dt-filter-btn-on', choiceSel[i] != null);
    sync();
    choiceSync.push(sync);
    return holder;
  }

  function buildChoicePanel(i, col) {
    const values = uniqueValues(col);
    const allowed = (low) => choiceSel[i] == null || choiceSel[i].has(low);
    const panel = el('div', { class: 'dt-choice-panel', onclick: (e) => e.stopPropagation() });

    const childCbs = [];
    const allCb = el('input', { type: 'checkbox', class: 'dt-check' });
    const refreshAll = () => {
      const n = values.filter((v) => allowed(v.low)).length;
      allCb.checked = n === values.length;
      allCb.indeterminate = n > 0 && n < values.length;
    };
    const commit = () => { page = 0; saveState(); render(); };

    allCb.addEventListener('change', () => {
      choiceSel[i] = allCb.checked ? null : new Set(); // all vs none
      childCbs.forEach((cb) => { cb.checked = allCb.checked; });
      allCb.indeterminate = false;
      commit();
    });
    // A search box for long value lists (many assignees / tags) - filters which
    // checkboxes are shown; it doesn't change what's selected.
    const optRows = [];
    if (values.length > 10) {
      panel.appendChild(el('input', {
        class: 'dt-choice-search', type: 'text', placeholder: 'search…', 'aria-label': 'Search values',
        oninput: (e) => {
          const q = e.target.value.trim().toLowerCase();
          for (const o of optRows) o.el.style.display = (!q || o.text.includes(q)) ? '' : 'none';
        },
      }));
    }
    panel.appendChild(el('label', { class: 'dt-choice-opt dt-choice-all' },
      [allCb, el('span', {}, 'Select all')]));
    panel.appendChild(el('div', { class: 'dt-choice-sep' }));

    const list = el('div', { class: 'dt-choice-list' });
    for (const v of values) {
      const cb = el('input', { type: 'checkbox', class: 'dt-check' });
      cb.checked = allowed(v.low);
      cb.addEventListener('change', () => {
        const s = choiceSel[i] == null ? new Set(values.map((x) => x.low)) : choiceSel[i];
        if (cb.checked) s.add(v.low); else s.delete(v.low);
        // Full set == no filter (null); keeps the funnel un-highlighted when all are on.
        choiceSel[i] = s.size === values.length ? null : s;
        refreshAll();
        commit();
      });
      childCbs.push(cb);
      const label = el('label', { class: 'dt-choice-opt' }, [cb, el('span', {}, v.display)]);
      optRows.push({ el: label, text: v.display.toLowerCase() });
      list.appendChild(label);
    }
    panel.appendChild(list);
    refreshAll();
    return panel;
  }

  // ── Cell rendering + inline editing ──
  function cellContent(col, row) {
    return col.render ? col.render(row) : String(valueOf(col, row) ?? '');
  }

  // (Re)fill a cell from its column's render. Used on first paint and after an
  // edit commits/cancels.
  function setCell(td, col, row) {
    clear(td);
    td.classList.remove('dt-saving');
    const c = cellContent(col, row);
    for (const k of Array.isArray(c) ? c : [c]) {
      if (k == null) continue;
      td.appendChild(typeof k === 'string' ? document.createTextNode(k) : k);
    }
  }

  // Make an editable cell enter edit mode on click (a `<select>` for `select`
  // columns, a chip editor for `chips`). Clicks here stop propagation so they
  // don't also toggle row selection.
  function makeEditable(td, col, row) {
    td.classList.add('dt-editable');
    if (!td.title) td.title = 'Click to edit';
    td.addEventListener('click', (e) => {
      if (e.target.closest('a') || td.querySelector('.dt-editor')) return;
      e.stopPropagation();
      if (col.edit.type === 'select') openSelectEditor(td, col, row);
      else if (col.edit.type === 'chips') openChipsEditor(td, col, row);
    });
  }

  async function runCommit(td, col, row, value) {
    clear(td).appendChild(el('span', { class: 'muted' }, 'Saving…'));
    td.classList.add('dt-saving');
    try {
      await col.edit.commit(row, value);
    } catch (err) {
      toast('Update failed: ' + (err?.message || err), true);
    }
    setCell(td, col, row); // reflects the (possibly-changed) row, or reverts
  }

  function openSelectEditor(td, col, row) {
    const cfg = col.edit;
    const cur = cfg.get ? cfg.get(row) : valueOf(col, row);
    const options = typeof cfg.options === 'function' ? cfg.options(row) : (cfg.options || []);
    const all = (!cur || options.includes(cur)) ? options : [cur, ...options];
    const sel = el('select', { class: 'dt-editor dt-editor-select' });
    all.forEach((o) => {
      const opt = el('option', { value: o }, o);
      if (o === cur) opt.selected = true;
      sel.appendChild(opt);
    });
    clear(td).appendChild(sel);
    sel.focus();
    let settled = false;
    sel.addEventListener('change', () => {
      if (settled) return;
      settled = true;
      if (sel.value === cur) setCell(td, col, row);
      else runCommit(td, col, row, sel.value);
    });
    sel.addEventListener('blur', () => { if (!settled) { settled = true; setCell(td, col, row); } });
    sel.addEventListener('keydown', (e) => { if (e.key === 'Escape') { e.stopPropagation(); settled = true; setCell(td, col, row); } });
  }

  function openChipsEditor(td, col, row) {
    const cfg = col.edit;
    const cur = (cfg.get ? cfg.get(row) : []).slice();
    const list = el('div', { class: 'dt-chip-list' });
    const drawChips = () => {
      clear(list);
      cur.forEach((t, i) => list.appendChild(el('span', { class: 'tag dt-chip-edit' }, [
        t, el('button', { type: 'button', class: 'dt-chip-x', onclick: (e) => { e.stopPropagation(); cur.splice(i, 1); drawChips(); } }, '×'),
      ])));
    };
    const input = el('input', { class: 'dt-chip-input', type: 'text', placeholder: cfg.placeholder || 'add tag…' });
    // Optional autocomplete from a "good data" list (e.g. sanctioned tags). A
    // native <datalist> keeps freetext fully available - it only suggests.
    let dataList = null;
    const suggestions = typeof cfg.suggestions === 'function' ? cfg.suggestions(row) : cfg.suggestions;
    if (suggestions && suggestions.length) {
      const id = 'dt-dl-' + Math.random().toString(36).slice(2);
      dataList = el('datalist', { id });
      suggestions.forEach((s) => dataList.appendChild(el('option', { value: s })));
      input.setAttribute('list', id);
    }
    const addTag = () => {
      const v = input.value.trim();
      if (v && !cur.some((x) => x.toLowerCase() === v.toLowerCase())) { cur.push(v); drawChips(); }
      input.value = '';
    };
    let settled = false;
    const cancel = () => { if (!settled) { settled = true; setCell(td, col, row); } };
    const save = () => { if (settled) return; addTag(); settled = true; runCommit(td, col, row, cur); };
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') { e.preventDefault(); addTag(); }
      else if (e.key === 'Escape') { e.stopPropagation(); cancel(); }
    });
    drawChips();
    // Stop clicks inside the editor from bubbling to the cell's open-on-click
    // listener - without this, Save/Cancel remove the editor and the same click
    // then re-opens it (Cancel would appear to do nothing).
    const editor = el('div', { class: 'dt-editor dt-editor-chips', onclick: (e) => e.stopPropagation() }, [
      list,
      el('div', { class: 'dt-chip-controls' }, [
        input,
        dataList,
        el('button', { type: 'button', class: 'btn btn-primary btn-xs', onclick: (e) => { e.stopPropagation(); save(); } }, 'Save'),
        el('button', { type: 'button', class: 'btn btn-xs', onclick: (e) => { e.stopPropagation(); cancel(); } }, 'Cancel'),
      ]),
    ]);
    clear(td).appendChild(editor);
    input.focus();
  }

  function cycleSort(i) {
    if (sortIndex === i) {
      if (sortDir === 1) sortDir = -1;      // asc -> desc
      else { sortIndex = null; sortDir = 1; } // desc -> unsorted
    } else { sortIndex = i; sortDir = 1; }   // new column -> asc
    saveState();
    render();
  }

  function resetFilters() {
    // Clears filters + choice selections; leaves the sort alone (that's what the
    // column header toggles, and users expect "Clear" to mean "clear the filters").
    filters.fill('');
    choiceSel.fill(null);
    filterRow.querySelectorAll('.dt-filter-input').forEach((inp) => { inp.value = ''; });
    choiceSync.forEach((fn) => fn());
    page = 0;
    saveState();
    render();
  }

  // Whether the view deviates from its pristine defaults (any filter, any choice
  // selection, or a sort other than the configured initial one). Drives whether the
  // persisted entry is worth keeping - a plain default view stores nothing.
  function hasNonDefaultState() {
    if (filters.some((f) => f)) return true;
    if (choiceSel.some((s) => s != null)) return true;
    const initIdx = opts.initialSort?.index ?? null;
    const initDir = opts.initialSort?.dir ?? 1;
    return sortIndex !== initIdx || (sortIndex != null && sortDir !== initDir);
  }

  // ── Cross-refresh persistence (no-op unless opts.persistKey is set) ──
  function saveState() {
    if (!persistKey) return;
    try {
      // Nothing to remember once the view is back to defaults - drop the entry so we
      // don't leave empty state lying around (and so `loadState` stays cheap).
      if (!hasNonDefaultState()) { localStorage.removeItem(persistKey); return; }
      localStorage.setItem(persistKey, JSON.stringify({
        v: 1,
        cols: columns.length, // guard: ignore a stale entry if the columns changed
        filters,
        choice: choiceSel.map((s) => (s == null ? null : [...s])),
        sort: sortIndex == null ? null : { index: sortIndex, dir: sortDir },
      }));
    } catch { /* localStorage unavailable / private mode / quota - persistence is best-effort */ }
  }

  function loadState() {
    if (!persistKey) return;
    let data;
    try { data = JSON.parse(localStorage.getItem(persistKey) || 'null'); } catch { data = null; }
    if (!data || data.cols !== columns.length) return;
    if (Array.isArray(data.filters) && data.filters.length === columns.length) {
      data.filters.forEach((f, i) => { filters[i] = String(f || ''); });
    }
    if (Array.isArray(data.choice) && data.choice.length === columns.length) {
      data.choice.forEach((c, i) => { choiceSel[i] = Array.isArray(c) ? new Set(c) : null; });
    }
    if (data.sort && Number.isInteger(data.sort.index) && data.sort.index < columns.length) {
      sortIndex = data.sort.index;
      sortDir = data.sort.dir === -1 ? -1 : 1;
    }
  }

  // Header checkbox: select or clear every currently-filtered row (all pages).
  function toggleAll() {
    const keys = lastShown.map(keyOf);
    const allSelected = keys.length > 0 && keys.every((k) => selected.has(k));
    if (allSelected) keys.forEach((k) => selected.delete(k));
    else keys.forEach((k) => selected.add(k));
    render();
  }

  function toggleRow(key, on) {
    if (on) selected.add(key); else selected.delete(key);
    updateSelectionUI();
  }

  // Selection count + clear link + header checkbox state (checked/indeterminate).
  function updateSelectionUI() {
    if (!selectable) return;
    selCount.textContent = selected.size ? `${selected.size} selected` : '';
    selClear.style.display = selected.size ? '' : 'none';
    const keys = lastShown.map(keyOf);
    const inSel = keys.filter((k) => selected.has(k)).length;
    if (headCheckbox) {
      headCheckbox.checked = keys.length > 0 && inSel === keys.length;
      headCheckbox.indeterminate = inSel > 0 && inSel < keys.length;
    }
    // Let the host view react (e.g. show a bulk-action bar) to selection changes.
    if (typeof opts.onSelectionChange === 'function') opts.onSelectionChange(selected.size);
  }

  function anyFilterActive() {
    return filters.some((f) => f) || choiceSel.some((s) => s != null) || sortIndex != null;
  }

  function compute() {
    let out = rows;
    if (opts.predicate) out = out.filter(opts.predicate);
    const active = filters.map((f, i) => (f ? i : -1)).filter((i) => i >= 0);
    if (active.length) {
      out = out.filter((row) => active.every((i) => {
        const col = columns[i];
        // A column may define its own match (e.g. multi-term AND, keywords);
        // otherwise use the default OR/negation match on its display value.
        return col.filterMatch
          ? col.filterMatch(row, filters[i])
          : matchesFilter(String(valueOf(col, row) ?? '').toLowerCase(), filters[i]);
      }));
    }
    // Checkbox (choice) filters: keep a row if ANY of its value(s) for the column is
    // in that column's allow-set (so a multi-tag item matches if it has a picked tag).
    const activeChoice = choiceSel.map((s, i) => (s != null ? i : -1)).filter((i) => i >= 0);
    if (activeChoice.length) {
      out = out.filter((row) => activeChoice.every((i) =>
        choiceValuesOf(columns[i], row).some((v) => choiceSel[i].has(v.toLowerCase()))));
    }
    if (sortIndex != null) {
      const col = columns[sortIndex];
      const numeric = col.sort === 'number';
      out = out.slice().sort((a, b) => {
        let av = valueOf(col, a);
        let bv = valueOf(col, b);
        if (numeric) {
          av = Number(av); bv = Number(bv);
          if (Number.isNaN(av)) return 1;      // missing values sort last
          if (Number.isNaN(bv)) return -1;
          return (av - bv) * sortDir;
        }
        return String(av ?? '').toLowerCase().localeCompare(String(bv ?? '').toLowerCase()) * sortDir;
      });
    }
    return out;
  }

  function render() {
    arrows.forEach((arrow, i) => {
      arrow.textContent = sortIndex === i ? (sortDir === 1 ? ' ▲' : ' ▼') : '';
      headCells[i].classList.toggle('dt-active', sortIndex === i);
    });
    const shown = compute();
    lastShown = shown;
    const total = shown.length;

    // Paginate the filtered+sorted set. pageSize 0 = all on one page.
    const pageCount = pageSize ? Math.max(1, Math.ceil(total / pageSize)) : 1;
    if (page > pageCount - 1) page = pageCount - 1;
    if (page < 0) page = 0;
    const start = pageSize ? page * pageSize : 0;
    const pageRows = pageSize ? shown.slice(start, start + pageSize) : shown;

    clear(tbody);
    for (const row of pageRows) {
      const tr = el('tr');
      if (selectable) {
        const key = keyOf(row);
        const cb = el('input', {
          type: 'checkbox', class: 'dt-check',
          onchange: (e) => {
            toggleRow(key, e.target.checked);
            tr.classList.toggle('dt-row-selected', e.target.checked);
          },
        });
        cb.checked = selected.has(key);
        if (selected.has(key)) tr.classList.add('dt-row-selected');
        tr.appendChild(el('td', { class: 'dt-check-cell' }, cb));
        // Clicking anywhere on the row toggles selection too - but let real
        // controls inside the row (links, buttons, the checkbox) work normally.
        tr.classList.add('dt-selectable-row');
        tr.addEventListener('click', (e) => {
          if (e.target.closest('a, button, input, select, label')) return;
          const on = !selected.has(key);
          toggleRow(key, on);
          cb.checked = on;
          tr.classList.toggle('dt-row-selected', on);
        });
      }
      for (const col of columns) {
        const td = el('td', col.class ? { class: col.class } : {});
        if (col.align) td.style.textAlign = col.align;
        setCell(td, col, row);
        if (col.edit) makeEditable(td, col, row);
        tr.appendChild(td);
      }
      tbody.appendChild(tr);
    }

    // Empty state: keep the table (and its per-column filter inputs) on screen
    // so the user can adjust a query that filtered everything out - just show a
    // placeholder row in the body instead of removing the whole table.
    if (!total) {
      const colspan = columns.length + (selectable ? 1 : 0);
      tbody.appendChild(el('tr', { class: 'dt-empty-row' },
        el('td', { class: 'empty dt-empty-cell', colspan: String(colspan) }, opts.emptyText || 'No matching rows.')));
    }

    // Count: "a-b of Y" when paginated + filtered, else the shown/total shape.
    const filtered = total !== rows.length;
    if (pageSize && total > pageSize) {
      count.textContent = `${start + 1}–${start + pageRows.length} of ${total}${filtered ? ` (of ${rows.length})` : ''}`;
    } else {
      count.textContent = filtered ? `${total} of ${rows.length}` : `${rows.length}`;
    }
    clearLink.style.display = anyFilterActive() ? '' : 'none';
    choiceSync.forEach((fn) => fn()); // keep funnel highlight in sync with state

    // Pager visibility + state.
    pager.style.display = pageCount > 1 ? '' : 'none';
    pageLabel.textContent = `Page ${page + 1} of ${pageCount}`;
    prevBtn.disabled = page <= 0;
    nextBtn.disabled = page >= pageCount - 1;

    updateSelectionUI();
  }

  wrap.refresh = render;
  // Swap the underlying data in place and re-render, KEEPING filters, sort, page,
  // and selection (selection is by key, so still-present rows stay selected). Lets a
  // caller refresh row data after a mutation without rebuilding the whole view (and
  // losing the user's filter). Mutates the array in place so all closures see it.
  wrap.setRows = (next) => {
    rows.length = 0;
    rows.push(...(next || []));
    render();
  };
  // Expose the current selection for callers that act on it (bulk operations).
  wrap.getSelection = () => rows.filter((r) => selected.has(keyOf(r)));
  // The current filtered+sorted set (ALL matching rows, not just the visible page) -
  // for exporting exactly what the user has narrowed to.
  wrap.getVisibleRows = () => lastShown.slice();
  wrap.clearSelection = () => { selected.clear(); render(); };
  render();
  return wrap;
}
