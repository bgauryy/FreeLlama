# MCP offload check

serve http://127.0.0.1:11455; 2026-10-31T22:26:30.180Z

## decision: issue file as state

```text
[strands-decider:2b-nvfp4] crash: true (p(true) 0.90)
kind: bug (p 0.95, confidence 0.79)
```
ok · 1487 ms

## embedding nearest: which programs hold the most memory when a model load has to wait

```text
closest of 678 items to the query:
0.727  packages/rust-core/src/platform/residency.rs:L1-31
0.724  packages/rust-core/src/platform/preload.rs:L287-343
0.723  packages/rust-core/src/platform/resources/mod.rs:L131-166
```
ok · 33588 ms · glob packages/rust-core/src/platform/**/*.rs

## embedding nearest: how a decision task request is built for /v1/systemone

```text
closest of 678 items to the query:
0.681  packages/rust-core/src/platform/execution/request.rs:L323-366
0.644  packages/rust-core/src/platform/tasks_api.rs:L33-61
0.633  packages/rust-core/src/platform/routing/mod.rs:L356-402
```
ok · 34301 ms · glob packages/rust-core/src/platform/**/*.rs

## embedding nearest: duplicate pairs

```text
most similar pairs among 4 items:
0.901  input[0]  ~  input[2]
0.759  input[1]  ~  input[3]
```
ok · 38 ms

## vision: screenshot by path

```text
[glm-ocr:latest · loaded 2.1 s] ```bash
RUN  v5.0.5 /Users/bgaryy/code/FreeLlama
[freellama serve] store 3 ms
[freellama serve] background start failed: freellama serve exited before becoming ready at
[freellama serve] store 3 ms
Test Files  29 passed (29)
    Tests  838 passed (838)
  Start at  18:06:36
  Duration  4.51s (tests 86%, import 10%, transform 3%, worker 1%)
```
```bash
RUN  v5.0.5 /Users/bgaryy/code/FreeLlama
[freellama serve] store 3 ms
[freellama serve] background start failed: freellama serve exited before becoming ready at
[freellama serve] store 3 ms
Test Files  29 passed (29)
    Tests  838 passed (838)
  Start at  18:06:36
  Duration  4.51s (tests 86%, import 10%, transform 3%, worker 1%)
```
```
[freellama: 190 repeated characters omitted here (full text in structuredContent); the model looped until its output limit — treat as low confidence]
```
ok · 58937 ms

## refusal: file outside roots

```text
file /etc/hosts is outside the readable roots (/Users/bgaryy/code/FreeLlama, /private/var/folders/nf/tf_qg9k56l97qnkcc2b_w5z40000gn/T); copy it into the project or temp directory, or add its directory to FREELLAMA_FILE_ROOTS
```
ERROR · 3 ms

