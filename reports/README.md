# Validation results

[validation.json](validation.json) records the tested build, input hashes and
results from a Pi Zero 2 W with a 240×280 Whisplay LCD on November 20, 2026.

Formatting, strict Clippy, all 15 Rust tests, the release build, and CLI checks in
simulated and hardware modes passed. Emoji/text acceptance checks also passed in
both modes.

## Extended playback

One daemon ran for 30 minutes 52 seconds with 1,815 GIF submissions and 443 clears.
The run covered continuous playback, regular switching, rapid replacement, a
near-limit cache, an over-budget streaming GIF, and a final cleared idle hold.

| Workload | Median RSS | CPU, % of one core |
| --- | ---: | ---: |
| Continuous full-screen GIF | 8.32 MiB | 3.35% |
| Regular switching and clearing | 15.96 MiB | 17.87% |
| Rapid replacement and clearing | 16.89 MiB | 30.33% |
| Cleared and idle | 16.90 MiB | 0.00% |

Peak RSS was 18.77 MiB, with zero swap, seven threads and fourteen file descriptors.
After warmup, cleared-state RSS changed by 0 KiB during regular switching and
8 KiB during rapid replacement. Freed allocator storage remained resident;
no sustained memory accumulation was observed in this run.

These are measurements for the recorded inputs and build, not guarantees for all
media or unlimited runtimes. See the [validation instructions](../tests/README.md)
to reproduce the checks, including the extended playback run.
