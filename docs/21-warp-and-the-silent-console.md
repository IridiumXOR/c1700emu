# 21. Warp mode and the "silent console"

Chapter 15.5 told of a **warp** idle mode (idle time skipped with no
wall-clock limit) that was removed because, after a minute of virtual time
racing ahead, IOS stopped answering the console. The cause wasn't found
then. This chapter finds it. Warp is back: `--idle warp`.

```
c1700emu --idle warp ...    # idle time flies: protocol timers, long uptimes in seconds
```

## 21.1 Reproducing it

`tools/tests/warp_test.py SNAPSHOT SECONDS` restores a snapshot of IOS at
the prompt (chapter 20) in warp mode, reads `show clock`, stays quiet for
the given time, then probes the console again.

The first attempts didn't reproduce anything, and showed something else:
virtual time moved barely faster than the wall clock. **Idle detection
wasn't running at all** after a restore. The snapshot had been saved from
a run with `--idle off`, and restoring it copied that run's "no anchor,
no candidates" over the restoring run's configuration. That was a real
bug in the snapshot code: what is detected is configuration (it comes from
the command line), only the detector's progress is state. `Idle` now
remembers its anchor choice; a restore keeps a recognised anchor when the
configuration allows one and otherwise re-arms the search
(`idle::tests::restore_keeps_the_configured_idle_detection`).

With idle detection really working, warp skipped ~1700 s of virtual time
in 30 s, and the console went silent: reproduced, every time.

## 21.2 Eliminating suspects

| hypothesis | experiment | result |
|---|---|---|
| the timebase's lower half (TBL, 2³² ticks = 23.9 min at 3 MHz) wraps during the warp, and something mishandles it | restore in **realtime** mode with the timebase set 15 s before the wrap (a temporary hook), wait across it | console fine |
| the same, while the console is logged out | log out, cross the wrap, press RETURN | console fine |
| the UART loses characters that arrive in a burst | read the model: the receive interrupt is raised whenever the FIFO holds anything | not possible |
| warp itself | warp, `exit`, wait 5/15/30 s, press RETURN | console fine |

So neither the timebase nor warp broke anything. What differed: the
failing test typed `show clock` as its first input after the quiet period,
the passing ones a bare RETURN.

## 21.3 What really happens

The console output during the quiet period, which the test now records,
tells the story:

```
\r\n\r\n ... \r\n
Router con0 is now available

Press RETURN to get started.
```

1. **IOS logs the console out after 10 minutes without input**: the
   default `exec-timeout 10 0` of `line con 0`. In warp, ten virtual
   minutes pass in about ten seconds.
2. A logged-out line runs no exec, so **nothing typed is echoed**.
3. The line is activated by a RETURN on its own; a whole line typed at
   "Press RETURN" (`show clock` + RETURN) is swallowed silently, in
   realtime mode just the same (`PROBE="show clock"
   tools/tests/exec_timeout_test.py SNAPSHOT`: no answer to the line, the
   next RETURN gives `Router>`).

The original warp session typed commands at a logged-out console, which
looks exactly like a dead router. It was IOS behaving normally, at a
speed that made its 10-minute timeout feel instantaneous.

## 21.4 Using warp

* Press RETURN first after a long idle period, or disable the timeout in
  IOS: `line con 0`, `exec-timeout 0 0`.
* Warp is for watching time-driven behaviour quickly (routing protocol
  timers, ARP/CDP ageing, long uptimes); realtime (the default) for
  everything interactive or networked, where the router's clock should
  match the world's.

Measured (`tools/tests/warp_test.py`, interpreter): 1367 s of virtual time
in 30 s of wall time (×46), console answering afterwards.

**Tests**

* `tools/tests/warp_test.py SNAPSHOT [SECONDS]`: warp, quiet period,
  RETURN, `show clock`; reports the speed-up.
* `tools/tests/exec_timeout_test.py SNAPSHOT [-- options]`: log out with
  `exit`, press RETURN, expect `Router>`; `PAUSE=N` waits between the two,
  `PROBE=text` types a line instead of a bare RETURN.
