# rewind

A time machine for your project folder. Every save, every AI-agent edit, every script
run gets recorded locally, and any of it can be taken back. No commits, no staging, and
it works with any editor, any agent, git or no git.

Git only knows what you commit. Editor "local history" only sees that editor. Agent
checkpoints only cover that agent. `rewind` watches the folder itself, so it catches
everything, and it treats a whole agent session as one undoable step.

## Install

```sh
cargo build --release
install -Dm755 target/release/rewind ~/.local/bin/rewind
```

## Five-minute tour

```sh
cd my-project
rewind init                    # start tracking (honors .gitignore)

rewind run -- claude           # run any agent / codemod / script under rewind
                               # → prints every file it changed when it exits
rewind diff -1                 # review what it did, line by line
rewind undo                    # didn't like it? the whole run is gone in one step
rewind goto 2                  # changed your mind again? jump to any snapshot

rewind watch                   # in a spare terminal: snapshot every save, forever
rewind log                     # the timeline
rewind log src/app.ts          # only snapshots touching one file
rewind restore src/app.ts      # put one file back to before its last change
rewind restore src --at 30m    # put a folder back to how it was 30 minutes ago
rewind cat src/app.ts --at 12  # print an old version without touching anything
rewind status                  # what changed since the last snapshot
rewind snap -m "works now"     # named checkpoint
rewind prune --older-than 7d   # drop old history
```

## Snapshot refs

| ref       | meaning                                   |
|-----------|-------------------------------------------|
| `12`      | snapshot #12 (see `rewind log`)           |
| `last`    | newest snapshot                           |
| `-1`      | one before the newest, `-2` two before…   |
| `10m`     | newest snapshot at least 10 minutes old (`s m h d w`) |

## How undo works

- Uncommitted edits are saved first, so `undo` with unsaved changes throws those away
  (recoverably). Run it again to keep going back.
- Everything recorded during one `rewind run` counts as one change-set.
- `undo`, `goto` and `restore` are snapshots themselves. Nothing is ever lost until you
  `prune`. Each one prints the command that takes you back.
- A file older than the snapshot you return to is never deleted. Example: an agent
  deletes `.gitignore`, so your existing `.env` suddenly becomes visible. Undo leaves
  `.env` alone.

## What is tracked

Every file not ignored by `.gitignore` (git repo or not) or by `.rewindignore`
(same syntax; it wins over `.gitignore`, so `!.env` adds history for `.env`). Always
skipped: `.git node_modules target .venv venv __pycache__ .next .nuxt .svelte-kit
.turbo .cache .gradle` and friends, files over 20 MB, symlinks. `rewind init`
refuses to start in `$HOME`, `/`, or folders with more than 20 000 files unless
`--force`.

## Using it with Claude Code

Use `rewind run -- claude`. Or add a hook so every tool call gets a checkpoint
(`~/.claude/settings.json`):

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Edit|Write|MultiEdit|Bash",
        "hooks": [{ "type": "command", "command": "rewind snap -q 2>/dev/null || true" }] }
    ]
  }
}
```

## Storage

Everything lives in `.rewind/` at the project root. That folder holds its own
`.gitignore`, so git never sees it.

- `journal.jsonl`: append-only, one snapshot per line, only the changes.
- `objects/`: zstd-compressed file contents, deduplicated by blake3 hash.
- `index.json`: stat cache, so a scan of 5 000 files takes about 30 ms.
- `lock`: lets a running `watch` and your commands safely share the folder.

To stop tracking, delete `.rewind/`.
