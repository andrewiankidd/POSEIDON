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

export function dataTable(columns, rows, opts = {}) {
  const filters = columns.map(() => '');
  let sortIndex = opts.initialSort?.index ?? null;
  let sortDir = opts.initialSort?.dir ?? 1; // 1 asc, -1 desc
  let page = 0;
  const pageSize = opts.pageSize > 0 ? opts.pageSize : 0; // 0 = show all

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
      fth.appendChild(el('input', {
        class: 'dt-filter-input', type: 'text',
        placeholder: col.filterPlaceholder || 'filter', 'aria-label': `Filter ${col.label}`,
        title: col.filterMatch ? undefined : 'comma = match any (active, resolved); ! = exclude (!closed)',
        oninput: (e) => { filters[i] = e.target.value.trim().toLowerCase(); page = 0; render(); },
      }));
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

  function valueOf(col, row) {
    return col.value ? col.value(row) : '';
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
    render();
  }

  function resetFilters() {
    filters.fill('');
    filterRow.querySelectorAll('input').forEach((inp) => { inp.value = ''; });
    page = 0;
    render();
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
    return filters.some((f) => f) || sortIndex != null;
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
