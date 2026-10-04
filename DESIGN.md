---
name: Rise
description: The web dashboard for Rise, an internal platform for deploying container images.
colors:
  rise-indigo: "oklch(0.55 0.17 270)"
  rise-indigo-soft: "oklch(0.95 0.04 270)"
  on-indigo: "#ffffff"
  daylight: "oklch(0.985 0.009 270)"
  desk: "#ffffff"
  shelf: "oklch(0.96 0.012 270)"
  shelf-hover: "oklch(0.945 0.013 270)"
  rule: "oklch(0.84 0.005 80)"
  rule-faint: "oklch(0.89 0.01 270)"
  ink: "oklch(0.20 0.006 80)"
  ink-muted: "oklch(0.50 0.006 80)"
  ink-soft: "oklch(0.62 0.006 80)"
  healthy: "oklch(0.62 0.13 150)"
  healthy-wash: "oklch(0.96 0.04 150)"
  caution: "oklch(0.62 0.12 70)"
  caution-wash: "oklch(0.96 0.06 80)"
  failing: "oklch(0.58 0.18 25)"
  failing-wash: "oklch(0.96 0.04 25)"
  after-hours: "oklch(0.17 0.008 270)"
  console: "oklch(0.205 0.01 270)"
  raised-console: "oklch(0.24 0.012 270)"
  rise-indigo-night: "oklch(0.70 0.15 270)"
  rise-indigo-night-soft: "oklch(0.30 0.07 270)"
  ink-night: "oklch(0.95 0.005 270)"
  log-surface: "oklch(0.967 0.004 80)"
typography:
  page-title:
    fontFamily: "Inter, -apple-system, BlinkMacSystemFont, system-ui, sans-serif"
    fontSize: "23px"
    fontWeight: 600
    lineHeight: 1.15
    letterSpacing: "-0.025em"
  title:
    fontFamily: "Inter, system-ui, sans-serif"
    fontSize: "15px"
    fontWeight: 600
    lineHeight: 1.4
  panel-title:
    fontFamily: "Inter, system-ui, sans-serif"
    fontSize: "13px"
    fontWeight: 600
    letterSpacing: "-0.005em"
  body:
    fontFamily: "Inter, -apple-system, BlinkMacSystemFont, system-ui, sans-serif"
    fontSize: "13.5px"
    fontWeight: 400
    lineHeight: 1.5
    letterSpacing: "-0.005em"
  meta:
    fontFamily: "Inter, system-ui, sans-serif"
    fontSize: "12.5px"
    fontWeight: 400
    lineHeight: 1.45
  label:
    fontFamily: "Inter, system-ui, sans-serif"
    fontSize: "11px"
    fontWeight: 600
    letterSpacing: "0.08em"
  mono:
    fontFamily: "JetBrains Mono, ui-monospace, SFMono-Regular, Menlo, monospace"
    fontSize: "12.25px"
    fontWeight: 400
    letterSpacing: "-0.01em"
    fontFeature: "liga 0"
rounded:
  chip: "3px"
  control: "4px"
  block: "5px"
  panel: "6px"
  card: "8px"
  dialog: "10px"
  sheet: "14px"
spacing:
  "1": "4px"
  "2": "6px"
  "3": "8px"
  "4": "10px"
  "5": "12px"
  "6": "14px"
  "7": "16px"
  "8": "18px"
  "9": "20px"
  "10": "22px"
  "11": "26px"
  "12": "28px"
  "13": "36px"
components:
  button-primary:
    backgroundColor: "{colors.rise-indigo}"
    textColor: "{colors.on-indigo}"
    rounded: "{rounded.control}"
    padding: "7px 12px"
    typography: "{typography.meta}"
  button-secondary:
    backgroundColor: "{colors.desk}"
    textColor: "{colors.ink}"
    rounded: "{rounded.control}"
    padding: "7px 12px"
  button-secondary-hover:
    backgroundColor: "{colors.shelf-hover}"
  button-danger:
    backgroundColor: "{colors.failing}"
    textColor: "{colors.on-indigo}"
    rounded: "{rounded.control}"
    padding: "7px 12px"
  field:
    backgroundColor: "{colors.desk}"
    textColor: "{colors.ink}"
    rounded: "{rounded.control}"
    padding: "8px 11px"
  status-healthy:
    backgroundColor: "{colors.healthy-wash}"
    textColor: "{colors.healthy}"
    rounded: "{rounded.control}"
    padding: "2px 9px"
  status-in-progress:
    backgroundColor: "{colors.rise-indigo-soft}"
    textColor: "{colors.rise-indigo}"
    rounded: "{rounded.control}"
    padding: "2px 9px"
  status-failing:
    backgroundColor: "{colors.failing-wash}"
    textColor: "{colors.failing}"
    rounded: "{rounded.control}"
    padding: "2px 9px"
  status-inactive:
    backgroundColor: "{colors.shelf}"
    textColor: "{colors.ink-soft}"
    rounded: "{rounded.control}"
    padding: "2px 9px"
  env-tag:
    backgroundColor: "{colors.shelf}"
    textColor: "{colors.ink-muted}"
    rounded: "{rounded.chip}"
    padding: "0 6px"
  env-tag-production:
    backgroundColor: "{colors.rise-indigo-soft}"
    textColor: "{colors.rise-indigo}"
    rounded: "{rounded.chip}"
    padding: "0 6px"
  nav-item-active:
    backgroundColor: "{colors.rise-indigo-soft}"
    textColor: "{colors.rise-indigo}"
    rounded: "{rounded.control}"
    padding: "7px 10px"
  panel:
    backgroundColor: "{colors.desk}"
    rounded: "{rounded.panel}"
  card:
    backgroundColor: "{colors.desk}"
    rounded: "{rounded.card}"
    padding: "14px 16px"
---

# Design System: Rise

## Overview

**Creative North Star: "The Control Room"**

Rise's dashboard is a calm operations surface for developers checking on their apps. The chrome stays quiet: cool paper surfaces separated by hairlines, a single indigo voice, and sharp 4–6px corners. Colour carries meaning, not decoration. Green, indigo, red and grey mark state, and indigo also marks the one action that matters on a screen. You should be able to read a page the way you read a status board: what is serving, whether it is healthy, what changed, and what to do next.

Density is deliberate. Base type is 13.5px, rows are compact, and identifiers, URLs and values are set in JetBrains Mono so they read as data. The layout is a fixed 240px sidebar that switches between global and project scope, a 52px top bar with breadcrumbs and the command palette, and a content column capped at 1200px. Below 768px the same system becomes a phone app: a header with a back button, a bottom tab bar, cards instead of tables, and dialogs as bottom sheets.

Light and dark are equal, with dark ("After Hours") a blue-grey night palette rather than inverted greys. Users may switch the accent hue in their profile (mint, ember, slate). Indigo is the canonical accent; the others only swap the accent and its light-mode tint, so everything must work under all of them.

**Key Characteristics:**
- Flat surfaces separated by 1px faint borders and tonal steps; shadows only on things that float.
- One accent: primary action, active navigation, focus, in-progress state, production.
- Four status tones shared by every badge and dot: healthy, in progress, failing, inactive.
- Monospace for facts (ids, digests, URLs, keys, values, logs, durations), Inter for everything else.
- Crisp, compact controls: 32px on desktop, 44px touch targets on phones.

## Colors

A cool-tinted paper palette with one indigo voice and a small, strict set of status colours.

### Primary
- **Rise Indigo**: the product's only accent. Primary buttons, the active nav item and its 3px marker, links, focus rings, in-progress statuses, the production environment chip, selected choices.
- **Rise Indigo Soft**: the tint behind indigo text. Active nav background, in-progress badges, the production chip, selected rows in pickers, the "After" side of a rollback/promote diff.
- **On Indigo**: text on indigo and red fills.

### Neutral
- **Daylight**: the page background behind every panel.
- **Desk**: panels, cards, tables, inputs, the sidebar and top bar.
- **Shelf**: table headers, inactive badges, chips, the live-deployment block inside an environment card, segmented-control troughs.
- **Shelf Hover**: row and button hover.
- **Rule**: control borders (buttons, inputs).
- **Rule Faint**: hairlines between panels, rows, header and content.
- **Ink**, **Ink Muted**, **Ink Soft**: primary text; secondary text and meta; captions, placeholders, column headers and section labels.
- **Log Surface**: the inset background of log consoles and log excerpts, a step below Daylight.

### Status
- **Healthy** on **Healthy Wash**: running, healthy, available.
- **Failing** on **Failing Wash**: failed, unhealthy, error banners, destructive actions.
- **Caution** on **Caution Wash**: warnings, protected-secret badges, "missing" counts, edited-variable notes.
- In progress uses Rise Indigo; inactive (stopped, superseded, cancelled, expired) uses Ink Soft on Shelf.

### Dark ("After Hours")
- **After Hours** (page), **Console** (panels, sidebar), **Raised Console** (Shelf equivalent), **Ink Night** (text), **Rise Indigo Night** and **Rise Indigo Night Soft** (accent and its tint). Status colours lighten and their washes darken correspondingly in `rise.css`.

### Named Rules
**The State Colour Rule.** Green, indigo, red and grey mean healthy, in progress, failing and inactive everywhere, through `statusTone`. Never use them decoratively, and never invent a fifth status colour.

**The One Voice Rule.** Indigo is the only accent. If two indigo things compete on a screen, one of them is not the primary action.

**The Environment Colour Exception.** Environments carry their own user-chosen colour (green, blue, yellow, red, purple, orange, gray), but only as the small layer glyph next to the environment's name. Environment colours never fill surfaces or carry status.

## Typography

**Display Font:** none. The system has no display face; page titles are Inter at 600.
**Body Font:** Inter (with -apple-system, system-ui fallback), self-hosted at weights 400–700.
**Label/Mono Font:** JetBrains Mono (with ui-monospace fallback), self-hosted at 400–600, ligatures off.

**Character:** a neutral, tightly tracked grotesque for interface text paired with a technical mono for every fact a developer might copy, compare or grep.

### Hierarchy
- **Page title** (600, 23px, 1.15, -0.025em): one per page, the section or project name. 21px on phones.
- **Title** (600, 15px): environment card names, variable scope headings, card titles.
- **Panel title** (600, 13px): panel and dialog headings inside a page.
- **Body** (400, 13.5px, 1.5): default text, list rows, table cells (13px).
- **Meta** (400, 12.5–12.75px): page subtitles, meta rows, buttons (12.75px, 500), captions.
- **Label** (600, 11px, 0.08em, uppercase): section labels such as "Environments", "Recent deployments", "Live deployment", "Rollout". Labels name the content directly below them; they are not eyebrows above a heading.
- **Mono** (12–12.5px): deployment ids, image tags, digests, URLs, variable keys (600) and values, log lines (12px, 1.65), durations.

### Named Rules
**The Facts in Mono Rule.** Anything a developer would copy or compare is set in JetBrains Mono: ids, image references, URLs, keys, values, commands. Prose and labels never are.

**The Tabular Numbers Rule.** Ages, durations, counts and table cells use tabular numerals so columns line up.

## Layout

- **Desktop shell:** a 240px sidebar (padding 16px 12px 12px, 18px gaps) and a main column with a 52px top bar (padding 0 28px). The content area has 28px 36px 48px padding and an inner max-width of 1200px (Home 1120px, new project 760px).
- **Sidebar modes:** global (Home, Projects, Teams, plus up to four recent projects) and project (back link, project switcher card, one item per project section with counts).
- **Spacing rhythm:** 4/6/8/10/12/14/16/18/20/22/26/28/36px. Rows inside a group sit 10–12px apart, groups 20–26px.
- **Grids:** content grids use `repeat(auto-fit, minmax(min(100%, Npx), 1fr))`: environment cards 320px, attention cards 300px, project cards 190px, template and choice cards 200px. Mid-widths reflow without extra breakpoints.
- **One breakpoint, 767px:** below it the shell becomes a 56px header (back chevron or brand, title with status dot and section sub-line, theme and search buttons) plus a bottom tab bar (min 54px, safe-area aware). Project tabs: Overview, Deploys, Envs, Vars, More (a bottom sheet with the rest). Content padding drops to 16px 16px 28px, tables become stacked cards, inputs go to 16px text, tab strips and filter segments scroll sideways.

## Elevation & Depth

Flat by default. Surfaces separate through tone (Daylight → Desk → Shelf) and 1px Rule Faint hairlines, never through resting shadows. Shadows appear only on elements that float above the page.

### Shadow Vocabulary
- **Segment lift** (`box-shadow: 0 1px 2px rgba(20,20,15,.04)`): the active option of a segmented control.
- **Dropdown** (`box-shadow: 0 12px 32px oklch(0.2 0.02 270 / .18)`): pickers, comboboxes, menus, the mobile "More" sheet (upward).
- **Dialog** (`box-shadow: 0 20px 60px oklch(0.2 0.02 270 / .25)`): dialogs, the command palette and toasts.
- **Focus** (`0 0 0 3px` of the accent at 16%): focused inputs and selected choice cards. Buttons and links use a 2px accent outline at 2px offset.

### Named Rules
**The Flat-at-Rest Rule.** Nothing on the page casts a shadow unless it floats: dialogs, palette, dropdowns, toasts. Cards and panels separate by border and tone alone.

## Shapes

Sharp, small corners in a strict ladder: 3px for chips, kbd and environment tags; 4px for buttons, inputs, nav items and status badges; 5px for inner blocks (live-deployment block, palette rows, segmented troughs); 6px for panels, tables and banners; 8px for environment cards and mobile cards; 10px for dialogs and the palette; 14px top corners for bottom sheets. Status dots are 6–7px circles; avatars are 4px-rounded squares in the sidebar and circles in team rosters. Borders are always 1px.

## Components

### Buttons
Crisp and compact.
- **Shape:** 4px corners, 1px border, 7px 12px padding, 12.75px/500 text, 7px icon gap, never wrapping. 44px minimum height on phones.
- **Primary:** Rise Indigo fill, white text, no border; hover brightens slightly. One per screen region.
- **Secondary:** Desk fill, Rule border, Ink text; hover Shelf Hover.
- **Danger:** Failing fill, white text, for confirming destructive actions only. Delete triggers inside dialogs are ghost buttons with Failing text, placed on the left of the footer.
- **Ghost / icon:** transparent, 28–32px square icon buttons for row actions (reveal, edit, delete, logs, settings); 40–44px on phones.
- **Row action:** a small outlined button ("Roll back here", "Set value", "Redeploy") whose border and text turn indigo on hover.
- **Focus:** 2px indigo outline, 2px offset.

### Status badges and dots
- **Badge:** dot (6px) and label, 2px 9px padding, 4px corners, 11.75px/500, coloured by tone. In-progress dots pulse at 1.2s; the only other moving dot is the green Live indicator of a streaming log console. Healthy and failing dots are still. A badge can carry a more precise label than its status, e.g. "Deploy failed" for an environment still served by its previous deployment.
- **Dot:** 7px circle in sidebar, list rows and pickers, same tones.

### Chips
- **Environment tag:** 11px, 0 6px padding, 3px corners, Shelf background with the environment's colour as a 10px layer glyph; production uses Rise Indigo Soft and indigo text.
- **Group pill, environment pill, owner label:** the existing `BasePill`, `GroupPill`, `EnvPill` and `OwnerLabel` primitives. Owners always carry an icon: people for teams, person for users.
- **Variable badges:** "Secret" (Shelf, Ink Muted) and "Protected" (Caution Wash, Caution, lock glyph).

### Cards / Containers
- **Panel:** Desk, 1px Rule Faint border, 6px corners, no shadow; header 12px 18px with a hairline below.
- **Environment card:** 8px corners; head (name, badge, replica count), body (URL, live-deployment block on Shelf, failure note on Failing Wash, other active groups), footer (Promote primary, Roll back secondary, icon actions).
- **List rows:** 11px 16px padding, hairline separators, Shelf Hover on hover, the whole row is the link.
- **Banner:** Failing Wash with a red-tinted 1px border and 6px corners for failures, with the action on the right (stacked below on phones).

### Inputs / Fields
- **Style:** Desk fill, 1px Rule border, 4px corners, 8px 11px padding, 13.5px Inter (mono for keys, values and image references).
- **Focus:** border turns indigo and gains a 3px indigo halo at 16%.
- **Disabled:** Shelf fill, Ink Muted text.
- **Segmented control:** Shelf trough with 2px padding; the active option is Desk with the segment-lift shadow.

### Navigation
- **Sidebar item:** 7px 10px padding, 4px corners, 13.5px Ink Muted with a 16px stroke icon. Hover is Shelf Hover. Active is Rise Indigo Soft with indigo 600 text and a 3×14px indigo marker on the left edge. Counts sit right in a 10.5px Shelf pill.
- **Project switcher:** Shelf card with a status dot, project name (13.5/600) and an owner/status sub-line; opens the command palette scoped to projects.
- **Breadcrumbs:** 13px; links in Ink Muted, the current page in Ink 600, ids in mono.
- **Mobile tab bar:** 21px icons and 10.5px labels; the active tab is indigo 600.

### Signature: Deployment diagnosis
The failure panel and rollout timeline on a failed deployment. The panel header sits on Failing Wash and states "Failed at <step>", the backend's reason, and what is still serving. Below it is an inset Log Surface excerpt of the last warning and error lines, colour-coded by level. The rollout is a list of 20px circular marks (done = Healthy fill with check, failed = Failing fill with cross, running = pulsing indigo ring, pending = Rule ring, reused image = dashed ring) with mono durations on the right.

### Signature: Command palette
⌘K / Ctrl+K, the search trigger, the mobile Search tab, or the project switcher. 600px panel at 12vh, 10px corners, dialog shadow, 52px input row; flat results, each with an icon, label, secondary text and a kind label (Action, Project, Team, Go to). Full screen with Cancel on phones.

## Do's and Don'ts

### Do:
- **Do** colour every status through `statusTone` and the four tones: healthy, in progress (indigo), failing, inactive.
- **Do** set ids, image references, URLs, variable keys and values in JetBrains Mono, with tabular numerals for ages and durations.
- **Do** separate surfaces with 1px Rule Faint hairlines and the Daylight → Desk → Shelf tone steps.
- **Do** keep controls compact (32px desktop) and give them 44px touch targets below 768px.
- **Do** turn tables into stacked cards (14px padding, 8px corners) and dialogs into bottom sheets on phones.
- **Do** theme browser surfaces from the palette: selection, scrollbars, caret, focus rings.
- **Do** check every new screen under all four accent palettes and in dark mode.

### Don't:
- **Don't** add resting shadows to cards, panels or rows; only floating layers get shadows.
- **Don't** introduce a second accent colour, gradients, or decorative colour; colour is state.
- **Don't** fill surfaces with environment colours; they live only in the small layer glyph.
- **Don't** use corners outside the 3/4/5/6/8/10/14px ladder.
- **Don't** add thick coloured side stripes to cards, rows or callouts; the only marker is the 3px active-nav bar.
- **Don't** set prose, labels or buttons in monospace.
