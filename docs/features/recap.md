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
  which is where the taxonomy you maintain on the [Rules](rules.md) screen pays
  off - well-tagged items land in the right slide, untagged ones don't.
- **The slides** - the deck is built in a fixed order:
  - **Title** - the period label and the closed-item count.
  - **At a glance** - headline metrics: items closed and areas touched.
  - **One feature slide per top area** - the busiest `area:` buckets (up to six),
    each listing its closed items. Every item sits under a heading that names a
    work-item type: items whose parent is in the data group under it (**Epic: …**,
    **Feature: …**); every other item groups under its own type (**User Stories**,
    **Bugs**, **Spikes**, …). The type names come straight from the tracker's data, so
    a customised Azure DevOps process labels itself - there is no catch-all "Other"
    bucket. The blurb under the title is written by the
    AI (see below), falling back to a prompt to add the story yourself.
  - **By source** - a breakdown of closed items per `source:` tag.
  - **Before you present** - a checklist reminding you to replace the
    auto-generated highlights with the real narrative, add screenshots for the
    marquee items, and trim to a tight story.
- **Edit the preview** - the preview is WYSIWYG. Every closed item is listed (no cap),
  and you tidy the deck by hand: hover a row and click **×** to drop it (an emptied
  group's heading goes with it), and click a slide's summary to rewrite it (plain
  text; **Esc** or clicking away saves). Edits live in the deck itself, so **Download
  deck** exports exactly what you see, and a late AI summary never overwrites wording
  you've already changed. **Regenerate** (or changing the window) rebuilds the deck and
  discards all edits.
- **AI summaries** - the deck renders straight away with placeholder blurbs, then
  POSEIDON sends each area's closed work items (titles, types, parent names, plus the
  team background from Rules) to your configured AI model through the same backend
  as tag suggestions and field drafting, and swaps in a short upbeat, product-owner
  style "what we shipped" blurb per area slide. Grounded in the titles only - it is
  told not to invent metrics or customers. If no online model is configured, or it
  fails, the placeholders stay and a note says why; the deck never depends on it.
  **Regenerate** asks the model again.
- **Theme and Branding** - the **🎨 Theme** button sets the deck's three base colours
  (background, text, accent; the rest is derived) and **🏷 Branding** swaps the faint
  corner logo for your own (PNG / JPG / SVG / WebP, shrunk automatically), picks which
  corner it sits in (default **top right**) and how opaque it is (default **40%**). Both
  apply live and into the downloaded deck. They are **tenant settings**: saved to your
  config, and carried in the config export/import YAML under a `recap:` key
  (`theme: {bg, ink, accent}`, `logo` as a base64 data URL, `logo_position`,
  `logo_opacity`), so a re-import restores them. Imported values are validated - only
  `#rrggbb` colours, an image data URL, a real corner and 5-100% are kept.
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
