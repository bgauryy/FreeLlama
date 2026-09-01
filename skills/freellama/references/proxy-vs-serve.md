# Proxy or serve

Load when choosing a mode or a control route returns 404. Why: both default to port 11435 but expose
different contracts.

| Need | `proxy` | `serve` |
|---|---:|---:|
| Raw `/api/*` retry/backoff/timeout | Yes | Yes |
| Managed discovery, routes, tasks, health | No | Yes |
| CPU model assignment and feedback | No | Yes |

`run_task`, `delegate_research`, installed/resident models, and control health need `serve`.
Research-adapter model turns now use managed `coding` tasks so they share routing, independent
backend admission, physical-placement receipts, and feedback protection. `doctor`, direct
lifecycle tools, model detail/raw views, and library search do not require `serve`.

For dual backend work, configure `serve --cpu-upstream ... --cpu-model <exact-tag>`. Managed catalog
and tasks honor it; raw passthrough remains on the primary upstream. Require current health
contracts before trusting placement. `doctor` reads chip and host RAM through macOS `sysctl`, Linux
`/proc`, or Windows system APIs. Only a non-null `unified_memory_bytes` says that host memory is
known to be shared with the accelerator.

Retry implementation details and historical asymmetry: `assets/evidence/proxy-vs-serve.md`.

Next: backend choice → `references/resource-routing.md`; failure diagnosis →
`references/troubleshooting.md`.
