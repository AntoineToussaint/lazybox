---
title: Recover persistent state
description: Diagnose a database startup failure or unreadable workspace without risking worktrees.
---

lazybox refuses to replace an unreadable persistent database with an empty
in-memory store. If startup reports a state error, preserve the original file
before attempting repair.

## 1. Stop lazybox and copy the state directory

```sh
lazybox server stop
cp -a ~/.lazybox/v2 ~/.lazybox/v2.backup-$(date +%Y%m%d-%H%M%S)
```

If `LAZYBOX_HOME` is set, substitute that directory for `~/.lazybox`.

## 2. Check permissions and disk health

The process must be able to read and write `~/.lazybox/v2/state.db` and its
parent directory. Confirm that the filesystem is not full and that the database
path is a regular file rather than a directory.

```sh
ls -ld ~/.lazybox ~/.lazybox/v2 ~/.lazybox/v2/state.db
df -h ~/.lazybox
```

## 3. Check SQLite integrity

With the daemon stopped and a backup already made:

```sh
sqlite3 ~/.lazybox/v2/state.db 'PRAGMA quick_check;'
```

`ok` means the SQLite container is sound; an individual stored JSON record may
still be incompatible. Lazybox preserves those rows and emits a storage warning
to subscribers; the JSON workspace endpoint reports the same condition in its
`warnings` array. Keep `/tmp/lazybox.log` with the record key and error
when reporting that case privately through the
[security policy](https://github.com/AntoineToussaint/lazybox/blob/main/SECURITY.md)
if sensitive data may be involved.

## 4. Preserve worktrees

Do not delete `~/.lazybox/v2/worktrees` or the bare-clone cache as a database
repair step. Worktrees can contain uncommitted or unpushed changes and are
independent of whether their metadata row can currently be loaded.

If you must start from a new database, move `state.db` aside rather than
deleting the full state directory. Re-add projects, then use the in-app
worktree inspector before removing anything left orphaned.
