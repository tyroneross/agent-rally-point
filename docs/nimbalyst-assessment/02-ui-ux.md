# nimbalyst UI/UX patterns

Nimbalyst's design system centres on a `--nim-*` CSS variable layer (semantic, not presentational) that maps 1:1 to Tailwind utilities and propagates automatically through three built-in themes (light, dark, crystal-dark) plus unlimited extension themes. On top of that token foundation the app runs a coherent component library: a Discord-style project rail, a unified diff approval bar, a walkthrough/help callout system, collapsible session groups, a Kanban board view, and a command-palette (QuickOpen). The result is a dense, keyboard-navigable workspace that stays visually calm because chrome is minimal and status is expressed through colour and icons rather than badges or background fills.

---

## Design system observations (with citations)

### Token architecture
- **Three-tier backgrounds** via `--nim-bg` / `--nim-bg-secondary` / `--nim-bg-tertiary` create visual depth without colour noise. Primary content sits on `--nim-bg`; sidebars and panels on `--nim-bg-secondary`; nested panels and code blocks on `--nim-bg-tertiary`. Rule: `--nim-primary` is _never_ used as a container background — only for buttons and interactive accents.  
  Source: `docs/CSS_VARIABLES.md:14-23`, `docs/UI_PATTERNS.md:91-119`

- **Semantic text hierarchy**: `--nim-text` → `--nim-text-muted` → `--nim-text-faint` → `--nim-text-disabled`, mapped to Tailwind shorthands (`text-nim`, `text-nim-muted`, etc.).  
  Source: `tailwind.config.ts:73-86`

- **Status colours as text only**: `--nim-success / warning / error / info` are used for text and border colour, never as background fills on badges. Status badges use `color-mix(in srgb, var(--nim-warning) 15%, transparent)` for a translucent tint if any fill is needed.  
  Source: `components/AgenticCoding/SessionHistory.css:95-147`

- **No theme-specific selectors**: all components reference `--nim-*` variables directly; the CSS variable values switch at the `:root` level per theme. No `[data-theme="dark"] .my-component` patterns.  
  Source: `docs/DARK_MODE_GUIDE.md:37-55`

- **Tailwind conditional-class pattern**: mutually exclusive states (active/inactive, selected/unselected) use a ternary to apply disjoint class sets, not additive strings. This prevents CSS-ordering bugs.  
  Source: `docs/UI_PATTERNS.md:92-109`

- **Container queries over media queries**: all responsive layout uses `@container` because panels are resizable and viewport breakpoints are meaningless inside a split pane.  
  Source: `docs/UI_PATTERNS.md:6-20`

- **Global `user-select: none`** on `#root` with explicit `select-text` opt-in on editor/content areas only. UI chrome (buttons, sidebar items, tabs) is non-selectable by default.  
  Source: `docs/UI_PATTERNS.md:121-150`

- **Derived colour tokens**: extension themes only specify primitive overrides; the runtime derives table, code-block, toolbar, scrollbar, and terminal colours automatically via `color-mix` and semantic relationships.  
  Source: `docs/EXTENSION_THEMING.md:69-76`

### Component primitives
- **`nim-btn`, `nim-btn-primary`, `nim-btn-secondary`, `nim-btn-ghost`, `nim-btn-danger`, `nim-btn-icon`** — a complete button scale in `styles/components.css:74-215`, all theme-aware.
- **`nim-modal` / `nim-modal-header` / `nim-modal-body` / `nim-modal-footer`** — consistent dialog chrome with `bg-nim-bg-secondary` headers/footers to create depth contrast at `components.css:13-67`.
- **`nim-panel` / `nim-panel-header` / `nim-panel-body` / `nim-section-label`** — the workhorse layout primitive; header uses `--nim-bg-secondary` to distinguish from body at `components.css:300-332`.
- **`nim-input`** with `--nim-primary` focus ring and `--nim-text-faint` placeholder at `components.css:222-245`.
- **`nim-badge` / `nim-pill`** — text-scale badges, never background-heavy, at `components.css:250-295`.

### Animations (keyframes defined in tailwind.config.ts:94-128)
- `focus-flash`: subtle flash from `--nim-bg` → `--nim-bg-hover` → `--nim-bg` on focus (0.4s), non-jarring.
- `walkthrough-fade-in`: 0.2s ease-out appear for callout bubbles.
- `bash-dot-pulse`: triple-dot progress indicator for streaming state.

---

## UX patterns worth stealing

### 1. Sticky diff approval bar with per-change navigation
A `sticky top-0 z-[100]` bar mounts at the top of an editor whenever AI-written changes are pending. It shows: **author attribution** (AI session name + `ProviderIcon` + relative timestamp), **prev/next change navigation** (only when per-change granularity is supported), and **Revert / Keep** button pairs that escalate from per-change to all-changes. Accept is `--nim-primary` filled; Reject is `--nim-bg` / `--nim-border` outlined — visually clear asymmetry so the safe action is visually dominant. The bar is container-responsive using `@container/diff-header` breakpoints that collapse button labels at narrow widths.  
Source: `components/UnifiedDiffHeader/UnifiedDiffHeader.tsx:154-250`  
**Why it works**: surfaces decision cost (who changed what, when) at the moment of review without requiring a separate review flow; keyboard-navigable prev/next makes large diffs tractable.

### 2. Discord-style project rail with per-project accent colours
A 56px vertical rail shows open projects as circular avatar icons with initials. Active item: border-radius transitions from circle (50%) to squircle (14px) via 150ms ease, no background changes needed. Active indicator: a 4px left-edge pill (`::before` pseudo-element) in `--rail-item-accent`. Accent colour is deterministically derived from the workspace path via HSL hash so the same project always gets the same colour across the rail, session history card, and workspace header bar. Close button appears on hover as a small × in the corner.  
Source: `components/ProjectRail.css:1-272`, `components/ProjectRail.tsx:49-57`  
**Why it works**: provides multi-project switching in minimal chrome; colour continuity across surfaces (rail icon ↔ header bar ↔ session card) creates spatial memory without labels.

### 3. HelpTooltip wrapper — test-id keyed, centrally registered help content
Any element decorated with `data-testid` can be wrapped in `<HelpTooltip testId="...">` and will show a rich tooltip (title + markdown body + keyboard shortcut `<kbd>`) on hover after a 500ms delay. Content is registered in a single `HelpContent.ts` dictionary keyed by test-id, not inline in JSX. Tooltips suppress on click (5s cooldown) and after window focus regain (1s cooldown). Portal-rendered to avoid z-index conflicts.  
Source: `help/HelpTooltip.tsx:71-366`, `docs/HELP_WALKTHROUGHS.md:6-18`  
**Why it works**: decouples help text from component code; test-ids are stable across refactors; centralized registry means all UI copy is findable and auditable in one place.

### 4. Walkthrough callout system — declarative multi-step guides
`WalkthroughDefinition` is a typed, declarative spec: trigger condition (screen + custom predicate + delay + priority), ordered steps (each with a target element resolved by test-id, placement hint, title, markdown body, optional keyboard shortcut, optional action button, and a `wide` flag for content-heavy steps). The callout re-checks target validity every 500ms and updates its anchored position on resize/scroll. Target elements receive a `.walkthrough-target-highlight` class during the step. Navigation shows `{n} of {total}` in the footer. Done button turns green (`#10b981`) as a distinct "complete" signal.  
Source: `walkthroughs/types.ts:38-118`, `walkthroughs/components/WalkthroughCallout.tsx:86-333`  
**Why it works**: purely declarative definitions let PMs write walkthroughs without touching component code; highlight class creates visual anchor for "look here" without overlays; deconfliction (priority + cooldown) prevents multiple guides fighting.

### 5. Collapsible session groups with count badges and chevron animation
`CollapsibleGroup` is a headless component: `button` header with uppercase section label (`tracking-wide` + `text-nim-faint`), animated rotating chevron (`transition-transform duration-200 rotate-90`), and optional count in `text-nim-faint` at 10px. No border, no background fill on the header — just text colour contrast distinguishes section from content.  
Source: `components/AgenticCoding/CollapsibleGroup.tsx:1-44`  
**Why it works**: section count gives at-a-glance inventory; no box chrome means the list content dominates at ≥70% of vertical space.

### 6. Tab dirty indicator with state-priority ordering
`TabDirtyIndicator` renders a single `•` character with colour encoding: `--nim-primary` for AI-unaccepted changes (highest priority), `--nim-warning` for unsaved changes, `orange-500` for collab-unsynced. All three states reduce to a single dot — no text, no badge — so tab width is unaffected. States are surfaced via per-tab Jotai atom subscriptions so only the affected tab re-renders.  
Source: `components/TabManager/TabBar.tsx:17-35`  
**Why it works**: single affordance (dot + colour) communicates three distinct states without occupying tab label space; atom-per-tab prevents waterfall re-renders.

### 7. Session status indicator with priority-ordered icon set
`SessionStatusIndicator` in the session list item renders one of: animated `contact_support` icon (waiting for interactive prompt, amber pulse), spinning `progress_activity` (processing, primary blue), `help` icon (pending prompt), `schedule` icon (scheduled wakeup with overdue/upcoming colours), or filled `circle` (unread). Each state has a title tooltip. The priority ordering is encoded structurally — first `if` wins — so the most actionable state is never hidden by a less urgent one.  
Source: `components/AgenticCoding/SessionListItem.tsx:14-74`  
**Why it works**: one icon slot per list row keeps density high; priority ordering means the user always sees the state that requires the most immediate attention.

### 8. Workspace colour accent bar — deterministic HSL from path hash
A 3px full-width bar at the top of sidebar headers uses `hsl(${hue}, 65%, 55%)` where `hue = abs(pathHash) % 360`. The same function is called identically in the project rail icon and session history card, so visual identity is consistent across the entire app without any user configuration.  
Source: `components/WorkspaceSummaryHeader.tsx:4-13`, `components/AgenticCoding/SessionHistory.tsx:2926`  
**Why it works**: provides identity signal at zero cognitive cost; determinism means the colour is stable across restarts and never needs persistence.

### 9. Command palette (QuickOpen) with progressive disclosure search
`QuickOpen` mounts as `fixed top-[20%] left-1/2` with `bg-black/50` backdrop. By default it shows file name search; pressing Tab or clicking a hint upgrades to content-search across file bodies. A small hint button (`Tab — Search in file contents`) appears inline in the input after the user has typed a query, avoiding mode confusion without a visible toggle. Results use a 3px left border accent (not background highlight) on the active row.  
Source: `components/QuickOpen.tsx:459-543`  
**Why it works**: the Tab-to-upgrade pattern follows progressive disclosure (simple case first, power user path available but not prominent); backdrop reinforces modal context without a hard overlay.

---

## Adoptable for agent-astronomer

| Pattern | Concrete change in agent-astronomer | Effort | Calm Precision notes |
|---|---|---|---|
| **Semantic CSS variable layer** (`--nim-bg / secondary / tertiary`, status as text colour) | Replace ad-hoc Tailwind `gray-*` and `blue-*` colour references with a CSS variable layer in `globals.css`; wire to Tailwind via `theme.extend`. Agent-astronomer's Tailwind v4 supports `@theme` declarations natively — define `--aa-bg`, `--aa-bg-secondary`, `--aa-text-muted`, `--aa-border`, `--aa-primary`, `--aa-success/warning/error` there. | S | Fully aligned — enforces "status as text colour not badge" and eliminates hardcoded values. |
| **Three-tier background depth** (`bg` / `bg-secondary` / `bg-tertiary`) | Apply `--aa-bg-secondary` to sidebar (already exists in `components/Sidebar.tsx`) and settings panels; apply `--aa-bg-tertiary` to code blocks in skill/plugin detail views. Prevents the current flat grey appearance in nested UI. | S | Aligned — supports grouping via depth not per-item borders. |
| **Collapsible section groups** (chevron + count, no border chrome) | `/skills` and `/plugins` list views currently show flat rows. Add collapsible groups for categories (e.g., "build-loop", "ibr", "research" namespaces). Use the uppercase tracking-wide label pattern with count badge. | M | Aligned — reduces initial list density, count gives inventory at a glance, no chrome added. |
| **HelpTooltip / centralized help registry** | Create `lib/help-content.ts` keyed by data-testid; wrap nav sidebar items and action buttons in a `HelpTooltip` component. Copy the 500ms delay + click-cooldown + window-focus-cooldown pattern. | M | Aligned — no visible change until hover; removes need for any inline help text that adds chrome. |
| **Walkthrough callout system** | Port the declarative `WalkthroughDefinition` type and `WalkthroughCallout` component. Use it for the first-time `/skills` and `/plugins` onboarding flows currently missing. The system suppresses after completion and re-shows on version bump. | L | Aligned — non-modal, non-blocking; only appears when triggered; dismissable with Escape. |
| **Tab / item dirty indicator dot** | Skill cards in edit mode could show a single `•` in `--aa-warning` when there are unsaved changes, replacing any "Save" banner or status text. | S | Aligned — single character, no badge, theme-aware. |
| **Session status priority ordering** | The `/history` route shows Claude Code runs. Surface status via a single icon per row (running spinner → pending prompt icon → unread dot) using the same priority cascade. Currently no per-session status indicator exists. | M | Aligned — one icon slot maintains density. |
| **Workspace/entity accent colour from hash** | Plugin and skill cards could derive a 3px top border colour deterministically from `id` or `name` hash. Gives identity without user configuration or stored preferences. | S | Partially aligned — Calm Precision prefers grouping with a single border; a 3px top accent on a card is decorative but does not add a per-item border around list items, so it avoids the anti-pattern. Keep accent thin. |
| **Command palette pattern** | `/context` and `/library` routes have search inputs but no keyboard-invocable overlay. Add a `Cmd+K` palette (fixed-center, backdrop, Tab-to-upgrade) for global search across skills, plugins, and library entries. | M | Aligned — modal with backdrop signals context shift clearly; progressive disclosure hides content search until asked. |

---

## Not worth copying

1. **Electron-specific CSS (`-webkit-app-region: no-drag`, `overflow: hidden` on `#root`, IPC theme propagation to satellite windows)** — none of this applies in a Next.js 16 browser app. The `DARK_MODE_GUIDE.md` pattern of broadcasting theme via IPC is replaced in a web context by CSS media queries or a `ThemeProvider` writing a `data-theme` attribute to `<html>`.

2. **Project rail (Discord-style vertical icon strip)** — agent-astronomer has ≤10 routes and a conventional left sidebar. A multi-project icon rail would be inappropriate UX at that scale. The accent-colour pattern and initials-avatar technique are worth borrowing, but not the rail shell itself.

3. **Virtualized list (Virtuoso) for session history** — nimbalyst's session history can hold hundreds of entries across worktrees and blitz groups, requiring windowed rendering. Agent-astronomer's skill/plugin lists are ≤200 items max; standard `map()` rendering with a collapsible-group architecture is sufficient and avoids Virtuoso's layout constraints.
