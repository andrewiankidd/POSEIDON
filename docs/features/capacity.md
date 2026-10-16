# Capacity

How much demand a team faces and how much it gets through - without sprints,
story points, or anyone having to tag and organise things first. Everything is
computed from data the tracker already holds (created / closed dates, states,
assignees, pull requests, commit timestamps); tags and the AI model only
*enrich* it, and anything that cannot be classified is shown as unknown rather
than guessed.

## GUI

Open **Capacity** from the sidebar (hash route `#capacity`). Pick a window
(8 / 13 / 26 / 52 weeks) at the top; the team selector scopes it like every
other screen.

- **What the data says** - a handful of plain-language findings computed from the
  numbers (no AI involved), e.g. "211 items arrived and 163 finished; one
  clean-up week flatters the finish count". **✨ Write summary** asks the AI
  model to explain them in prose; it is given *only* those facts, labelled
  "AI-written", and you should read it before sharing. **⧉ Copy** puts the
  findings (and the summary, if written) on the clipboard.
- **Demand vs delivery** - arrivals against completions per week. Weeks where
  completions are far above normal (a bulk clean-up) are marked and excluded
  from the "typical week" figure, so a purge doesn't pass for capacity.
- **Where the demand comes from** - reactive (support, requests, incidents),
  planned (roadmap), maintenance (tech-debt), and unclassified, from `source:` /
  `initiative:` tags. **✨ Classify untagged demand** asks the AI model to
  classify what has no tag; verdicts are stored locally (never written to the
  tracker), marked as AI-derived in the "By AI" column, and only used when no tag
  already answers the question.
- **How long things take** - cycle time (creation to done) as the median, 85th
  and 95th percentile. Percentiles, not averages, so a few ancient items don't
  distort it; the 85% figure is a fair promise to quote.
- **In flight now** - work in progress, items untouched for 30+ / 90+ days,
  open work with no owner, and per-person load.
- **Running lists** - items that are a standing list rather than a deliverable
  ("running list", "ongoing", ...; or judged so by the AI) are left out of the
  numbers, because they never finish.
- **Knowledge concentration** - from merged pull requests: how few people account
  for half of them, and which repositories are effectively one person's
  ("single point"). This is the evidence for "why can't you just delegate it".
- **When people work** - a weekday × hour heat grid and per-person table from
  commit timestamps: after-hours share, late nights, weekend days, longest run
  of consecutive days, longest single day, days active out of days available,
  and an hour-of-day sparkline. Only people on the team roster are shown.
  **Each person is read in their own timezone** once you set one (see below);
  until then they are read in yours, which makes a colleague on the other side of
  the world look like they work all night.
- **Timezones & leave** - the editor under the working-hours table. Give each
  person an IANA timezone (`Europe/London`, `Asia/Manila`; daylight saving is
  handled per commit date) and add leave ranges (annual leave, public holidays,
  sick days). Leave comes off the days someone was expected to work, and any
  commits on a leave day are flagged ("worked 2") and called out in the
  findings. It is stored in your Poseidon config (so it exports/imports with the
  rest) and never written to the tracker.
- **About these numbers** - honest caveats (throughput counts items not effort;
  commits miss meetings, escalations and chat, so hours understate the load;
  how much was classified, and whether timezones and leave are set).

## Interruptions (Teams channels)

Work that arrives in chat never touches the backlog, so a card on the Capacity
screen measures it directly: how many conversations start in the Teams channels
a team watches, how many come from outside the team, how long the first answer
takes, who ends up answering, and what is still waiting. It needs no tagging.

- **Read-only, metadata only.** POSEIDON records who started each conversation,
  who replied and when. Message text is never stored (and nothing is ever posted
  to Teams). Bots and connectors (alert feeds) are counted separately and left
  out of the human numbers.
- **As you.** It reads Microsoft Graph with your own delegated token, so it sees
  only the channels you are a member of.
- **Per team.** Paste channel links into the team's profile (Settings → edit the
  team → *Teams channels*; in Teams, right-click a channel → *Get link to
  channel*). The same card appears under each team's Capacity view.
- **After hours** uses each poster's own timezone when one is set under
  *Timezones & leave*; **inside / outside the team** uses the team roster.

### One-off setup: a single-tenant app registration

Reading channel messages needs a Microsoft Entra app registration with admin
consent; there is no supported way around that. It is made once, in your own
tenant, and Poseidon ships with no ID baked in:

1. Entra admin centre → *App registrations* → *New registration*; supported
   account types: *this organisation only*.
2. *Authentication* → *Advanced settings* → *Allow public client flows*: **Yes**
   (no secret and no redirect URI are needed).
3. *API permissions* → *Microsoft Graph* → *Delegated*: `ChannelMessage.Read.All`,
   `Team.ReadBasic.All`, `Channel.ReadBasic.All`; then *Grant admin consent*.
4. Copy the *Application (client) ID* and *Directory (tenant) ID* into the card
   on the Capacity screen, save, and press **Connect Teams** (a device-code
   sign-in, like the Azure DevOps one).

The two IDs are not secrets and live in the config (so they export and import
with it). The sign-in token is cached on the data volume next to the Azure
DevOps one - never in the database - and renews silently.

Collection runs in the background after a poll (at most every 3 hours; **Read
channels now** forces it). The first read goes back 90 days; later reads re-check
the last two weeks, so answers to threads older than that are not picked up.

## AI

The two ✨ actions use the AI model configured in Settings → AI (an
OpenAI-compatible endpoint, or the local Claude Code CLI). With none enabled they
say so; everything else on the screen works without it. Classification is
batched (about 150 items per run - run it again for the remainder) and ids are
never echoed by the model, only item numbers, so a garbled reply can't
mislabel anything.

## Commit collection

Commit activity is gathered in the background after a poll (Azure DevOps today;
at most every 6 hours, last 120 days) and kept in the local database. It is
timestamps and author names only - no commit content.

## CLI

No dedicated command; this is a GUI view (also served at `/api/capacity`,
`/api/capacity/classify` and `/api/capacity/summary`).

## See also

- [Work Items](work-items.md) - the tags (`source:*`, `initiative:*`) that sharpen the demand mix.
- [Reports](reports.md) - date-range aggregates for specific questions.
- [Pull Requests](pull-requests.md) - the data behind knowledge concentration.
