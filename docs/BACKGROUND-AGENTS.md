# Running agents in the background and reattaching

A keepty session outlives the terminal, SSH connection or agent process that started it.
The broker owns the PTY, so a long job such as a coding agent, a test loop or a build keeps
running when every client disconnects, and you (or another agent) can reconnect later.

## Start detached

```bash
keepty start agent --detach -- codex
keepty start tests --detach -- bash -c 'cargo test 2>&1 | tee -a ~/logs/tests.log'
```

The child is spawned when the first Writer connects (`keepty attach`, `send`, `keys`, or an
SDK Writer). For a scripted job, attach once (`keepty attach tests`, then `,,,d`) or connect any Writer to kick it off.

## Check on it later

```bash
keepty list                          # running sessions
keepty screen agent                  # current screen, no resize, no redraw
keepty screen agent --scrollback     # plus retained scrollback (primary screen only)
keepty wait agent --quiet 2000       # block until output settles
keepty wait agent --text "DONE" --new --timeout 600
keepty attach agent                  # interactive; detach with ,,,d
keepty attach --watch agent          # read-only
```

`screen` and `wait` connect as Monitor, so checking a session never disturbs it. Reattaching as
a Writer replays the current screen and asks the program to redraw.

## Logs: keep a durable record

The broker keeps only a 1 MB ring of recent output in memory. For anything you may need after
the session ends, have the job write its own log:

```bash
keepty start loop --detach -- bash -c './run-loop.sh 2>&1 | tee -a ~/logs/loop.log'
tail -f ~/logs/loop.log            # from anywhere, no keepty client needed
```

A progress/checkpoint file that the job appends to after each step makes resuming trivial: on
reconnect, read the log first, then `keepty screen` the session.

## keepty session vs nohup

| | keepty session | `nohup cmd > log 2>&1 &` |
|---|---|---|
| Survives client/SSH disconnect | yes | yes |
| Interactive TUIs (codex, vim, htop) | yes, real PTY | no TTY; many TUIs refuse or misrender |
| Inspect live screen / send keys later | `screen`, `send`, `keys`, `attach` | no; only the log |
| Wait for output programmatically | `wait --text/--new/--quiet` | poll the log |
| Output history | 1 MB ring + your own `tee` log | the log file |
| Survives VM/container restart | no | no |

Use a keepty session for anything interactive or anything you want to drive later; `nohup`
is fine for plain batch commands that only need a log.

## After a disconnect

Nothing to do: the session is still running. `keepty list`, then `screen` or `attach`.

## After a VM or container restart

Processes do not survive a reboot, so the broker and its child are gone, and so is the
in-memory ring. Stale socket files are cleaned up by the next `keepty start` / `list`.
Recover from what is on disk:

1. Read the job's log / checkpoint file to see the last completed step.
2. Start a fresh session with the same id: `keepty start agent --detach -- codex resume --last`
   (use the agent's own resume command, if it has one).
3. Re-run only the steps after the last checkpoint.

For jobs that must come back automatically, wrap the `keepty start` in a systemd user unit or
an `@reboot` cron entry; keepty itself does not persist sessions across reboots.