# Asynchronous, batched decisions

**Status:** decided (conversation, 2026-09-02).
**Implemented by:** `docs/superpowers/plans/2026-09-02-async-action-queue.md`

> "Each time I confirm a particular file, regardless of within the merge
> editor or quick action button, the button swaps to 'working…' and I cannot
> confirm another file. I need to wait for that file to finish. I had told
> you that I wanted actions to be async: you confirm something, it goes to
> some queue and you can continue doing other stuff within the app."

## 1. What is wrong today

Three separate defects present as one experience.

**1.1 One boolean blocks the whole app.** `ReviewView::action_in_flight`,
`MergeView::saving` and `Shell::dashboard_action_in_flight` are single
booleans guarding an early `return`. Confirming file A does not merely
disable A's button — it makes every other file's button a no-op, and the
click is *discarded*, not remembered.

**1.2 Every file pays the whole tail.** Per decision, from the field log:
`chezmoi re-add` ≈ 2.5 s, `git push` ≈ 1.5 s, everything else milliseconds.
Both are per-*invocation* costs; both accept a list. Thirty files paid them
thirty times.

**1.3 Transient failures are terminal.** A templated file rendered through
`onepasswordRead` fails outright when `op` hiccups; retrying by hand works.
A daemon rescan holds the core lock for ~17 s and every IPC in a decision
(`session_start`, `snapshot_blobs`, `expect_changes`) waits on it — the
12-second gap in the log between `source-path` and the next command.

## 2. Decisions

**2.1 A serial queue with a non-blocking interface.** Work still runs one at
a time — every decision mutates one git repository (index, merge state,
chezmoi's own autoCommit/autoPush); two in parallel would contend for
`index.lock` and interleave commits. What changes is the *interface*: a
decision is recorded the instant it is made, its row moves to a
**Processing** section, and the user carries on. This is the user's own
model ("it goes to some queue").

**2.2 Batch the one-click decisions.** Consecutive `Keep disk` and
`Keep source · this machine` decisions drain as ONE batch:

```
per file  (ms, IPC)   session_start → snapshot_blobs → decision
once      (seconds)   expect_changes(all)
                      chezmoi re-add  <all keep-disk targets>
                      chezmoi apply   <all keep-source targets>
                      git commit · git push
per file  (ms, IPC)   session_end
once                  rescan
```

Mid-merge, a batch falls back to the single-file paths. Not because
`chezmoi re-add` cannot run over unmerged paths — it can, even with
autoCommit on (Task 1 proved the first draft of this sentence wrong) — but
because the shared tail stages the whole index, and mid-merge that would
commit conflicts the user never answered.

Merge-editor saves and `Keep source · origin` are queued (non-blocking)
but run singly: the former is inherently one-at-a-time (the user is reading
a diff), the latter loads origin content per file and is rare.

**2.3 Undo stays per file.** Each file keeps its own journal session, so
one-click Undo reverts one file exactly as today. Sessions are millisecond
IPC calls; the seconds are in `re-add` and `push`, which are what the batch
shares. (User's choice, 2026-09-02.)

**2.4 A re-queued target replaces its pending choice.** Choosing "keep
disk" then "keep source" before either ran must not apply the abandoned
choice first. Last one wins.

**2.5 Failures stick to their row.** The queue moves on within milliseconds
of an error; a transient banner would vanish unseen — which is how the
1Password flake became "random". A failed row shows the message in place,
its buttons re-enable, and choosing again retries.

**2.6 Confirming advances the selection.** After a decision the view selects
the next row that still needs one, so a sweep is one click per file with no
re-targeting click between.

**2.7 Retry transient 1Password failures.** `EvalFailureKind::OnePasswordAuth`
from a chezmoi call is retried a bounded number of times with a short
backoff, each attempt logged. Same shape as the `index.lock` retry
(0.1.16). `OnePasswordMultipleAccounts` is *not* transient and is not
retried.

**2.8 Never re-add with an empty target list.** To chezmoi that means
"every modified file" and would sweep unrelated drift into the source.
`re_add(&[])` is a hard error (shipped in the foundation commit).

## 3. Out of scope, deliberately

- Making the daemon's rescan faster, or not holding the core lock across
  it. The queue makes the wait *not block you*; the underlying ~17 s is a
  separate piece of work.
- Batching merge-editor saves or `Keep source · origin`.
- A batch-level "undo the whole sweep" (rejected in favour of 2.3).
