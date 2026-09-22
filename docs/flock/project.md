# Flock Project Config

This file tells Flock skills how this repository handles tracked work. Keep repository policy here, not machine-specific commands or personal preferences.

## Tracker

System: GitHub Issues
Repository: `andybarilla/herdr-scuttlebutt`
Default branch: `main`

Read issue:

```bash
gh issue view <number> --comments
```

List ready issues:

```bash
gh issue list --state open --label ready-for-agent --json number,title,labels,updatedAt,url --limit 50
```

Read PR:

```bash
gh pr view <number> --json number,title,body,state,author,url,headRefName,baseRefName,mergeable,reviewDecision,statusCheckRollup
```

Read PR diff:

```bash
gh pr diff <number>
```

Repository-specific tracker docs:
- `AGENTS.md`
- `docs/agents/issue-tracker.md`
- `docs/agents/triage-labels.md`

## Labels

State labels:
- ready-for-agent: `ready-for-agent`
- needs-triage: `needs-triage`
- needs-info: `needs-info`
- ready-for-human: `ready-for-human`
- blocked: no dedicated blocked label is configured; leave the issue open with a blocking comment and the most accurate existing state label
- wontfix: `wontfix`

Size labels:
- epic: no dedicated epic label or body convention is configured

Other labels of interest:
- bug: `bug`
- enhancement: `enhancement`
- documentation: `documentation`
- accessibility: `accessibility`
- help wanted: `help wanted`
- good first issue: `good first issue`
- question: `question`
- duplicate: `duplicate`
- invalid: `invalid`

## Branch

Default base branch: `main`
Issue branch pattern: `flock/issue-<number>-<short-slug>`
PR branch pattern: same as issue branch pattern unless the user requests otherwise

Before starting issue work:

```bash
git fetch origin
git checkout main
git pull --ff-only
git checkout -b flock/issue-<number>-<short-slug>
```

Notes:
- `main` exists locally and as `origin/main`.
- No repo-specific branch naming policy was found; this pattern is the Flock default for issue work.

## Gate

Commands that prove a worker's change locally:

```bash
scripts/check-versions.sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Notes:
- CI runs these checks on pull requests.
- CI test coverage runs `cargo test --locked` on Ubuntu and macOS; local workers usually run on the current platform only.
- Release builds are covered separately by `.github/workflows/release.yml` for tag/workflow-dispatch releases.
- `README.md` also documents `cargo test` and `cargo clippy --all-targets`; prefer the locked CI variants for Flock gates.

## PR

Open PRs with:

```bash
git push -u origin HEAD
gh pr create --fill
```

Tracker completion policy:
- Use neutral PR references such as `Refs #<number>`; do not rely on PR auto-close wording to complete issues.
- After successful issue work, explicitly comment with completion evidence and close the tracker item when policy allows.
- Close only after implementation, observed validation, PR/update preparation when applicable, and required review have succeeded with no blocking verdict.
- Leave the tracker item open when work is blocked, validation fails, review is blocking, or required review cannot run.
- Check commit messages for accidental closing keywords before opening the PR.

Notes:
- No repo-specific PR template or PR creation policy was found.
- PRs are not treated as a triage request surface per `docs/agents/issue-tracker.md`.

## Review

Default review tier: standard
Advanced review available: yes

Escalate to advanced review when:
- security/auth/permissions are touched
- data migrations, deletion, persistence, or log/state compatibility are touched
- concurrency, delivery, retry, restart, or state-machine behavior is touched
- public CLI, plugin manifest, storage layout, or documented compatibility behavior is touched
- tests are weak or acceptance criteria are unclear

Blocking bar:
- correctness bugs
- missing required behavior
- meaningful test gaps
- security/data-safety risk
- undocumented behavior changes to CLI, storage, delivery, grouping, or release/install flows

Notes:
- Domain docs are single-context per `docs/agents/domain.md`; read `CONTEXT.md` if present and relevant ADRs under `docs/adr/` before larger design work.

## Merge

Policy: human merges

Notes:
- Flock v1 workflows never auto-merge.
- No repo-specific auto-merge policy was found.
- If this changes later, record exact check-wait and merge commands here.

## Retry

Policy: no retry

Notes:
- Flock v1 issue-loop stops on blocked/failed work.
- No repo-specific retry policy was found.

## Operator Approval Policy

Mutating operator automation requires explicit approval policy here. When this section is absent, operator workflows must use dry-run only and ask before any mutation.

Approval categories:
- Issue selection for queued work: ask
- Grooming labels/comments: ask
- Triage labels/comments: ask
- Branch creation: ask
- Commits: ask
- PR creation/update: ask
- Tracker completion/issue close: ask
- Merge: never

Limits:
- Max cycles per operator run: 1
- Max issues worked per operator run: 1
- Max grooming batches per operator run: 0
- Max triage issues per operator run: 0
- Max runtime: ask

Stop conditions:
- missing or insufficient project config
- dirty or unexpected worktree state
- auth, branch, validation, PR, review, or tracker failure
- blocking product or technical question
- ambiguous, too broad, already complete, or non-dispatchable issue
- failed validation or blocking review
- configured limits reached

## Workflow Defaults

Ready queue label: `ready-for-agent`
Groom target depth: 6
Groom batch size: 10
Issue loop default limit: 1
Confirm before starting queued issue: yes

Defaults:
- Use GitHub issues as the source of truth.
- Prefer one issue per branch and one PR per issue unless the user asks otherwise.
- Read `AGENTS.md` before running Flock workflows.
- Use triage label mapping from `docs/agents/triage-labels.md`.

## Project Notes

- This is a Rust CLI/herdr plugin repository.
- `herdr-plugin.toml`, `Cargo.toml`, and release tags should stay version-aligned; `scripts/check-versions.sh` enforces this.
- CI is defined in `.github/workflows/ci.yml`.
- Release packaging is defined in `.github/workflows/release.yml`.
- Development commands documented in `README.md`: `cargo test`, `cargo clippy --all-targets`.
- Storage, delivery, grouping, and agent-pane behavior are domain-sensitive; prefer tests around behavior changes.
- Existing agent docs are authoritative for tracker usage and triage label names.
