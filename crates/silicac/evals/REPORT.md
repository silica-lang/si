# Agentic-eval report (audit #35 P7-7b, risk #5)

Runner: `silicac::eval`.  4 tasks (4 with a real agent `candidate.si`, the rest scored on the reference).  Metric: does the submission compile, and how often does it reach for a strictness escape hatch (casts, wrap/sat ops, `.raw`, endian)?

| task | kind | compiles | casts | wrap/sat | raw | endian | total |
|------|------|----------|-------|----------|-----|--------|-------|
| author_counter | author | yes | 0 | 0 | 0 | 0 | 0 |
| author_heartbeat | author | yes | 0 | 0 | 0 | 0 | 0 |
| debug_narrowing | debug | yes | 1 | 0 | 0 | 0 | 1 |
| edit_add_second_led | edit | yes | 0 | 0 | 0 | 0 | 0 |

## Summary

- Agent submissions passing the harness: **4/4**.
- Agent escape-hatch frequency: **1** total across 4 programs (casts 1, wrap/sat 0, **`.raw` 0**, endian 0).
- Reference baseline: 1 total (P7-7a).

**Risk #5 read:** agent output stays at the language's abstraction level — `.raw` frequency is **0** and the only escape hatch used is the one the debug task legitimately calls for (a single truncating `as` cast). The strictness defaults are not being routed around.
