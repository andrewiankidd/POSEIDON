// Minimal markdown-to-HTML renderer. Hand-rolled (no dep) because the
// docs subset we ship uses a known feature set: headings (h1-h3),
// paragraphs, ordered/unordered lists, inline `code`, fenced ```code
// blocks```, tables, blockquotes, **bold**, *italic*, links, hr.
//
// Why hand-rolled: marked.js is ~30KB of features we don't use, and a
// 200-line renderer is much easier to audit + keep XSS-free than
// pulling in a parser-generator.
//
// XSS posture: all docs ship from the repo (we author them, the build
// bundles them). We still escape user text in the HTML output as a
// defence against accidental HTML in a doc file leaking into the DOM as
// markup. URLs in `[text](url)` aren't validated beyond the
// "no javascript:" check below.

function escapeHtml(s) {
    return s
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
        .replace(/'/g, '&#39;');
}

/// Inline-markdown pass: code spans, bold, italic, links. Code spans
/// run FIRST so their contents (which often contain `*` and friends)
/// don't get re-interpreted as emphasis.
function renderInline(text) {
    const codePlaceholders = [];
    // Pull inline `code` spans out first; replace with placeholders so
    // their content isn't touched by emphasis/link passes.
    text = text.replace(/`([^`]+)`/g, (_, code) => {
        codePlaceholders.push(`<code>${escapeHtml(code)}</code>`);
        return ` CODE${codePlaceholders.length - 1} `;
    });
    text = escapeHtml(text);
    // Images ![alt](url) — must run BEFORE the link pattern, since
    // `![alt](url)` contains `[alt](url)` and would otherwise be
    // matched as a link with a stray `!` left in front. `javascript:`
    // is a no-op in <img src> on modern browsers but reject defensively
    // anyway, same posture as the link handler.
    text = text.replace(/!\[([^\]]*)\]\(([^)]+)\)/g, (_, alt, url) => {
        const safeUrl = /^javascript:/i.test(url.trim()) ? '#' : url;
        return `<img src="${safeUrl}" alt="${alt}">`;
    });
    // Links [text](url). Reject `javascript:` URLs defensively.
    text = text.replace(/\[([^\]]+)\]\(([^)]+)\)/g, (_, label, url) => {
        const safeUrl = /^javascript:/i.test(url.trim()) ? '#' : url;
        return `<a href="${safeUrl}">${label}</a>`;
    });
    // Bold **text**.
    text = text.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
    // Italic *text* — match `*text*` only when not abutting `**`.
    text = text.replace(/(^|[^*])\*([^*\n]+)\*(?!\*)/g, '$1<em>$2</em>');
    // Restore code spans.
    text = text.replace(/ CODE(\d+) /g, (_, i) => codePlaceholders[Number(i)]);
    return text;
}

function renderTableRow(line, isHeader) {
    const cells = line
        .replace(/^\||\|$/g, '')
        .split('|')
        .map((c) => c.trim());
    const tag = isHeader ? 'th' : 'td';
    return `<tr>${cells.map((c) => `<${tag}>${renderInline(c)}</${tag}>`).join('')}</tr>`;
}

/// Block-pass renderer. Walks the markdown line-by-line emitting
/// HTML. Lists and tables are paragraph-like in that they consume
/// consecutive matching lines; everything else is line-local.
export function renderMarkdown(md) {
    const lines = md.replace(/\r\n?/g, '\n').split('\n');
    const out = [];
    let i = 0;
    while (i < lines.length) {
        const line = lines[i];

        // Fenced code blocks (```lang ... ```)
        const fenceMatch = line.match(/^```(\S*)\s*$/);
        if (fenceMatch) {
            const lang = fenceMatch[1];
            const buf = [];
            i++;
            while (i < lines.length && !/^```\s*$/.test(lines[i])) {
                buf.push(lines[i]);
                i++;
            }
            i++; // skip closing fence
            const classAttr = lang ? ` class="lang-${escapeHtml(lang)}"` : '';
            out.push(`<pre><code${classAttr}>${escapeHtml(buf.join('\n'))}</code></pre>`);
            continue;
        }

        // Headings (h1-h3 only; deeper levels round to h3)
        const h = line.match(/^(#{1,6})\s+(.*)$/);
        if (h) {
            const level = Math.min(3, h[1].length);
            out.push(`<h${level}>${renderInline(h[2])}</h${level}>`);
            i++;
            continue;
        }

        // Horizontal rule
        if (/^---+\s*$/.test(line) || /^\*\*\*+\s*$/.test(line)) {
            out.push('<hr>');
            i++;
            continue;
        }

        // Tables: header row + separator + body rows
        if (/^\s*\|.*\|\s*$/.test(line) && i + 1 < lines.length && /^\s*\|\s*:?-+/.test(lines[i + 1])) {
            const header = renderTableRow(line, true);
            i += 2; // skip separator
            const bodyRows = [];
            while (i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i])) {
                bodyRows.push(renderTableRow(lines[i], false));
                i++;
            }
            out.push(`<table><thead>${header}</thead><tbody>${bodyRows.join('')}</tbody></table>`);
            continue;
        }

        // Blockquote (single-level)
        if (/^\s*>/.test(line)) {
            const buf = [];
            while (i < lines.length && /^\s*>/.test(lines[i])) {
                buf.push(lines[i].replace(/^\s*>\s?/, ''));
                i++;
            }
            out.push(`<blockquote>${renderInline(buf.join(' '))}</blockquote>`);
            continue;
        }

        // Unordered list
        if (/^\s*[-*]\s+/.test(line)) {
            const items = [];
            while (i < lines.length && /^\s*[-*]\s+/.test(lines[i])) {
                // Gather continuation lines (indented, no new list marker, non-blank).
                const buf = [lines[i].replace(/^\s*[-*]\s+/, '')];
                i++;
                while (
                    i < lines.length
                    && lines[i].trim() !== ''
                    && !/^\s*[-*]\s+/.test(lines[i])
                    && !/^\s*\d+\.\s+/.test(lines[i])
                    && !/^#{1,6}\s/.test(lines[i])
                    && !/^```/.test(lines[i])
                ) {
                    buf.push(lines[i].trim());
                    i++;
                }
                items.push(`<li>${renderInline(buf.join(' '))}</li>`);
            }
            out.push(`<ul>${items.join('')}</ul>`);
            continue;
        }

        // Ordered list
        if (/^\s*\d+\.\s+/.test(line)) {
            const items = [];
            while (i < lines.length && /^\s*\d+\.\s+/.test(lines[i])) {
                items.push(`<li>${renderInline(lines[i].replace(/^\s*\d+\.\s+/, ''))}</li>`);
                i++;
            }
            out.push(`<ol>${items.join('')}</ol>`);
            continue;
        }

        // Blank line → paragraph break (just skip)
        if (line.trim() === '') {
            i++;
            continue;
        }

        // Paragraph — consume consecutive non-blank, non-block lines.
        const para = [line];
        i++;
        while (
            i < lines.length
            && lines[i].trim() !== ''
            && !/^#{1,6}\s/.test(lines[i])
            && !/^```/.test(lines[i])
            && !/^\s*[-*]\s+/.test(lines[i])
            && !/^\s*\d+\.\s+/.test(lines[i])
            && !/^\s*>/.test(lines[i])
            && !/^---+\s*$/.test(lines[i])
            && !/^\s*\|.*\|\s*$/.test(lines[i])
        ) {
            para.push(lines[i]);
            i++;
        }
        out.push(`<p>${renderInline(para.join(' '))}</p>`);
    }
    return out.join('\n');
}
