---
name: monkado-release-notes
description: Write the Portuguese "Notas da versão" that Monkado's in-app update log shows. Use when opening or updating any application PR (fill its "Notas da versão" section), before cutting a release (review what the release step will publish), and when the owner asks to improve or fix a published release's changelog.
---

# Monkado release notes

The app's update screen shows the body of every release newer than the installed build, parsed
by `release_notes` in `apps/desktop/src/updater.rs`: `### Section` headings and `- item` bullets,
links reduced to their text, at most 60 items and 16 KiB per release. Anything else is dropped.
The owner reads these notes in Portuguese; write them for someone using Monkado, not for a
reviewer.

## Every application PR

Fill the PR template's `## Notas da versão` section, one bullet per user-visible change:

- Portuguese, present tense, what the user sees or can now do: "Clique na miniatura de uma
  imagem anexada para vê-la em tamanho real", not "add staged image viewer".
- Name the screen or action ("na call", "no compartilhamento de tela", "ao colar"), and the
  platform when only one is affected ("no Windows 10", "com placa AMD").
- One idea per bullet, no code identifiers, file paths, crate names, issue jargon or upstream
  PR numbers. Ports from Serein describe the Monkado behavior, not the port.
- A fix says what stopped going wrong ("a transmissão não cai mais quando..."), not the cause.
- At most 8 bullets per PR; group small related fixes into one.
- Leave the section empty for changes with no user-visible effect (CI, docs, refactors, tests).
  Never invent effects that were not verified; mark platform-limited or untested behavior
  honestly (for example "em prévia" for Preview SDK capabilities).

Updating an open PR body (`gh pr edit --body-file`) is part of ordinary delivery.

## Before a release

`tools/release-notes.py` (run by the Release workflow's "Escrever notas da versão" step) collects
the PRs squash-merged since the previous release, takes their "Notas da versão" bullets, groups
them by Conventional Commit type into `Novidades` (feat), `Correções` (fix), `Desempenho` (perf)
and `Outras mudanças`, appends `(#PR)` and falls back to the PR title when a PR has no notes.
Preview it before dispatching the release:

```powershell
python tools/release-notes.py monkey-land/monkado <previous-tag> main
```

If an item is a bare English title or misleading, fix that PR's "Notas da versão" (merged PR
bodies can still be edited) and preview again.

## Fixing a published release

When the owner asks to improve an existing changelog, write the notes file in the same shape
(`### Novidades` / `### Correções` / `### Desempenho`, `- item (#PR)`, then
`**Todas as mudanças**: <compare link>`), checking each item against what that tag actually
contains (`git log <previous-tag>..<tag>` and the PR diffs), and publish it with
`gh release edit <tag> -R monkey-land/monkado --notes-file <file>`. Do not change tags, assets or
titles. Read the published body back and report the release link.
