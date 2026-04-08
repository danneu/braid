TODO: What to do now that we've removed checkpoint system.

```
● Good question. Let me think through this honestly.

  What the checkpoint system is trying to do: recover from mid-operation
   interrupts by remembering which phase braid was in.

  The fundamental problem: braid doesn't own the state. The kernel does.
   When braid is killed, btrfs operations keep running. When braid
  restarts, the checkpoint says "resume at phase X" and blindly retries
  — but the world has moved on. The kernel might have finished, failed,
  or still be running. The checkpoint doesn't know.

  What you actually need is state detection, not state replay. Probe the
   current reality:
  - Is an exclusive operation already in flight? → attach/wait
  - Is a LUKS device open but not in the pool? → that's a partial add
  - Is a device gone from the pool but LUKS still open? → that's a
  partial remove

  You can infer all of this from live system state. You don't need a
  saved phase.

  Is it too complex? Yes, for braid's scale. A disk has ~4 lifetime
  events (create pool, add, maybe replace, maybe remove). The checkpoint
   system adds resume gates, fingerprint matching, phase hooks, args
  hashing, config hashing — and it still gets the hard cases wrong (like
   this one). The complexity budget is spent on infrastructure instead
  of on the actual hard problems (ENOSPC detection, attaching to
  in-flight ops, progress reporting).

  For v1 I'd suggest:
  1. Drop the checkpoint/resume system entirely
  2. Before any mutating operation, probe live state (btrfs, LUKS,
  exclusive ops in progress)
  3. If you detect a partial prior operation, tell the user what state
  things are in and what to do
  4. Pre-flight checks (like your new ENOSPC check) prevent the
  dangerous cases upfront

  The safety comes from understanding current state, not from replaying
  history.
```
