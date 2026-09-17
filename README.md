# keylock

Run a command behind an input lock, so a stray key can't interrupt it.

A multi-hour migration can be killed by one accidental Ctrl+C — or, if the
script reacts to single keys, by any key at all. `keylock` runs the command in
its own pseudo-terminal and sits in between: while the session is locked,
every key, Ctrl+C, mouse click and paste is dropped. Output always passes
through. It works in any terminal on macOS and Linux, including inside tmux,
Herdr and over SSH.

## Install

```sh
cargo install --git https://github.com/bayoudhi/keylock
```

## Use

```sh
keylock run --locked -- ./migrate.sh     # start locked
keylock run -- ./migrate.sh              # start unlocked, lock later
```

| To | Do |
|----|----|
| Lock from inside the terminal | Ctrl+] then `l` |
| Lock from another shell | `keylock on migrate.sh` |
| Unlock from inside the terminal | type `unlock` (no Enter needed) |
| Unlock from another shell | `keylock off migrate.sh` |
| See sessions | `keylock ls` |
| Check one session | `keylock status migrate.sh` |

While locked, the terminal title starts with 🔒, and typing rings the bell and
briefly shows how to unlock.

Options for `run`:

- `--locked` — start locked.
- `--name NAME` — session name (default: the command's file name; `-2`, `-3`, …
  is added if the name is taken).
- `--phrase PHRASE` — unlock phrase (default `unlock`, or `$KEYLOCK_PHRASE`).
- `--no-phrase` — only `keylock off` can unlock.
- `--no-hotkey` — pass Ctrl+] through to the command untouched.

To send a literal Ctrl+] to the command, press it twice.

The command sees `KEYLOCK_NAME` in its environment. `keylock` exits with the
command's exit code (128 + signal number if it was killed by a signal).

## Limits

- The phrase guards against accidents, not people: it is visible in `ps` when
  passed with `--phrase`.
- `keylock` can't stop the terminal, tab or pane itself from being closed.
- It only protects commands started through it.
- While locked, replies from the terminal are dropped too, so a program that
  queries the terminal (for example for the cursor position) gets no answer.

## License

MIT
