# Private audio transport validation — 2026-09-14

An isolated A/B probe created owned null sinks, captured their monitor with `Audio.read()`, and sent 144,000 bytes of synthetic mono PCM16 at 24 kHz through `Audio.write()` (three seconds). No hardware device or Discord route was connected. Both probes removed their owned processes/devices afterward.

| Null-sink driver priority | Result | Captured bytes | Peak |
|---|---|---:|---:|
| `priority.driver=0` | Write timed out after 5.67 seconds | 0 | 0 |
| `priority.driver=1` | Write completed in 3.07 seconds | 144,000 | 676 |

Only `priority.driver` changed between probes. `node.driver=true` alone did not clock the isolated graph when its driver priority was zero. Both configurations retained `priority.session=0`; neither selected a desktop default.

The production fix sets driver priority to 1, below physical device drivers. The full synthetic speech regression is `python tests/smoke_voice.py` from the voice directory, using the installed voice environment. It uses private buses and a separate local model socket.
