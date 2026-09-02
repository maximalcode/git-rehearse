# Issue tracker: GitHub

Issues and specs for this repo live as GitHub issues. Use the `gh` CLI for all
operations.

## Conventions

- **Create an issue**: `gh issue create --title "..." --body "..."`. Use a
  heredoc for multi-line bodies.
- **Read an issue**: `gh issue view <number> --comments`, filtering comments by
  `jq` and also fetching labels.
- **List issues**: `gh issue list --state open --json number,title,body,labels,comments --jq '[.[] | {number, title, body, labels: [.labels[].name], comments: [.comments[].body]}]'` with appropriate `--label` and `--state` filters.
- **Comment on an issue**: `gh issue comment <number> --body "..."`
- **Apply / remove labels**: `gh issue edit <number> --add-label "..."` /
  `gh issue edit <number> --remove-label "..."`
- **Close**: `gh issue close <number> --comment "..."`

Infer the repo from `git remote -v`; `gh` does this automatically inside this
clone.

## Pull requests as a triage surface

**PRs as a request surface: no.** _(Set to `yes` if this repo treats external
PRs as feature requests; `/triage` reads this flag.)_

When set to `yes`, PRs run through the same labels and states as issues, using
the `gh pr` equivalents. GitHub shares one number space across issues and PRs,
so resolve an ambiguous bare `#42` with `gh pr view 42` and fall back to
`gh issue view 42`.

## When a skill says “publish to the issue tracker”

Create a GitHub issue.

## When a skill says “fetch the relevant ticket”

Run `gh issue view <number> --comments`.

## Wayfinding operations

Used by `/wayfinder`. The **map** is a single issue labelled `wayfinder:map`,
holding the Notes / Decisions-so-far / Fog body. Create it with
`gh issue create --label wayfinder:map`.

- **Child ticket**: link it to the map as a GitHub sub-issue with `gh api`.
  Where sub-issues are unavailable, add the child to the map task list and put
  `Part of #<map>` at the top of its body. Use `wayfinder:<type>` labels
  (`research`, `prototype`, `grilling`, or `task`).
- **Blocking**: use GitHub's native issue dependencies. Add an edge with
  `gh api --method POST repos/<owner>/<repo>/issues/<child>/dependencies/blocked_by
  -F issue_id=<blocker-db-id>`, using the blocker's numeric database id rather
  than its issue number or node id. If dependencies are unavailable, use a
  `Blocked by: #<n>, #<n>` line at the top of the child body.
- **Frontier query**: list the map's open children, drop any with an open
  blocker or assignee, and take the first remaining ticket in map order.
- **Claim**: `gh issue edit <n> --add-assignee @me` is the session's first write.
- **Resolve**: comment the answer, close the issue, and append a context pointer
  (gist plus link) to the map's Decisions-so-far section.
