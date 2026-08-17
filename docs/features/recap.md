# Recap

A shareable highlights deck POSEIDON generates from your **closed** work over a
chosen window. It answers "what did we ship, and how do I show it?" - turning the
backlog you already track into the skeleton of a stakeholder update, so you're
editing a draft rather than starting from a blank slide. Merged in from a slide
tool called OCTOGON.

Recap builds the **data skeleton**; you finish the narrative and add the
screenshots. The story behind each item - why it mattered, what it unblocked -
isn't in the backlog, so the deck leaves clearly marked gaps for you to fill.

## GUI

Open **Recap** from the sidebar (hash route `#recap`), grouped with
[Reports](reports.md) under the sidebar's shareable-outputs section.

- **Window** - pick **Last 30 / 60 / 90 days**. Recap collects every work item in
  a resolved state (Closed / Done / Resolved / Completed / Removed) whose state
  changed within that window, then **Regenerate** rebuilds the deck.
- **Grouping** - closed items are bucketed by their `area:` and `source:` tags,
  and counted **internal vs external** (from the `internal` / `external` tags),
  which is where the taxonomy you maintain on the [Rules](rules.md) screen pays
  off - well-tagged items land in the right slide, untagged ones don't.
- **The slides** - the deck is built in a fixed order:
  - **Title** - the period label and the closed-item count.
  - **At a glance** - headline metrics: items closed, areas touched, internal,
    and external.
  - **One feature slide per top area** - the busiest `area:` buckets (up to six),
    each listing its closed items and a prompt to add the story.
  - **By source** - a breakdown of closed items per `source:` tag.
  - **Before you present** - a checklist reminding you to replace the
    auto-generated highlights with the real narrative, add screenshots for the
    marquee items, and trim to a tight story.
- **Download deck** - exports the deck as a **single self-contained HTML file**
  (deck data, slide renderer, and styles inlined). It opens and presents in any
  browser with no POSEIDON and no network - hand it to a stakeholder, drop it in a
  wiki, or present from it directly.

## CLI

No CLI surface - Recap is a GUI view. It reads the same stored work items the
rest of the app polls (refresh them with `poseidon poll`, then generate the deck
in the app).

## Where things live

- **Data** - computed on the fly from the stored work items (see the
  [User Guide](user-guide.md)); there's no separate recap store. The deck is a
  view over the same closed items, grouped by their `area:` / `source:` tags.
- **The exported file** - a one-off artifact you download and own; POSEIDON keeps
  no copy.

## See also

- [Reports](reports.md) - the other shareable output; flow metrics over a window.
- [Rules](rules.md) - the tag taxonomy (`area:` / `source:`) that Recap groups by.
- [Work Items](work-items.md) - the closed items the deck is built from.
