# LOCAL_TEST: what FreeLlama's local models can take off an agent's plate

100 graded questions, run one at a time through FreeLlama (`scripts/local-test/run.mjs`), each with a
known answer. Before each model group the harness asks FreeLlama (a route preview, which loads nothing)
whether the model fits in memory now, and waits instead of forcing a load. Machine: Apple M2 Pro,
32 GB unified memory, Ollama 0.40.13, FreeLlama 0.8.0 serve at http://127.0.0.1:11455. Results are one run on one
machine, not a benchmark.

| Group | Model | Result |
|---|---|---|
| Classification with embeddings | `nomic-embed-text:v1.5` | 18/20 passed, 0 pending |
| Classification with the decision model | `strands-decider:2b-nvfp4` | 18/20 passed, 0 pending |
| OCR | `glm-ocr:latest` | 30/30 passed, 0 pending |
| Small coding tasks | `gpt-oss:20b` | 0/0 passed, 25 held, 0 pending |
| Small research agent: delegate_research | `gpt-oss:20b` | 0/0 passed, 5 held, 0 pending |

Grading: OCR passes when every expected phrase appears in the transcript (case and spacing ignored);
classification when the top label equals the expected one; coding and agent answers when the
expected text (or pattern) appears in the answer. "held" means FreeLlama did not admit the model
within 30 minutes because the machine did not have room for it.

## Classification with embeddings (nomic-embed-text:v1.5)

| # | Question | Expected | Answer | Pass | ms |
|---|---|---|---|---|---:|
| E01 | Uploading a PDF freezes the browser tab | bug | bug (0.756 vs docs 0.705) | ✅ | 16 |
| E02 | Add keyboard shortcuts for switching projects | feature | feature (0.717 vs question 0.695) | ✅ | 18 |
| E03 | How can I invite a teammate to my workspace? | question | question (0.733 vs feature 0.701) | ✅ | 16 |
| E04 | The docs page for webhooks shows the wrong header name | docs | docs (0.765 vs question 0.687) | ✅ | 21 |
| E05 | Sync stops with error code 0x80070005 after the update | bug | bug (0.745 vs docs 0.690) | ✅ | 19 |
| E06 | Would love a Slack integration for notifications | feature | feature (0.781 vs question 0.661) | ✅ | 18 |
| E07 | Is there a limit on how many files I can store? | question | question (0.673 vs feature 0.656) | ✅ | 16 |
| E08 | The tutorial links to a page that no longer exists | docs | docs (0.807 vs bug 0.735) | ✅ | 14 |
| E09 | Search returns duplicate results for every query | bug | question (0.632 vs feature 0.621) | ❌ | 14 |
| E10 | Please support exporting charts as PNG images | feature | feature (0.753 vs docs 0.667) | ✅ | 15 |
| E11 | Which payment methods do you accept? | question | question (0.775 vs feature 0.718) | ✅ | 23 |
| E12 | The CLI reference does not mention the --verbose flag | docs | docs (0.756 vs question 0.654) | ✅ | 20 |
| E13 | Notifications arrive twice on Android | bug | bug (0.686 vs feature 0.660) | ✅ | 16 |
| E14 | Add a calendar view for tasks | feature | feature (0.723 vs question 0.683) | ✅ | 14 |
| E15 | Can I change my username after signing up? | question | question (0.761 vs feature 0.702) | ✅ | 15 |
| E16 | Code sample in the Python quickstart has a syntax error | docs | docs (0.732 vs bug 0.683) | ✅ | 25 |
| E17 | The progress bar stays at 99% forever | bug | docs (0.696 vs question 0.693) | ❌ | 17 |
| E18 | Offer a self-hosted version for enterprises | feature | feature (0.733 vs question 0.671) | ✅ | 15 |
| E19 | Do you have a student discount? | question | question (0.736 vs feature 0.655) | ✅ | 18 |
| E20 | Changelog entry for 2.1 is missing from the docs site | docs | docs (0.800 vs question 0.705) | ✅ | 21 |

## Classification with the decision model (strands-decider:2b-nvfp4)

| # | Question | Expected | Answer | Pass | ms |
|---|---|---|---|---|---:|
| D01 | Why did my credit card get billed two times this month? — Which team handles this ticket? | billing | billing | ✅ | 1854 |
| D02 | The app crashes every time I open the settings screen. — Which team handles this ticket? | technical | technical | ✅ | 200 |
| D03 | My parcel shows delivered but nothing arrived. — Which team handles this ticket? | shipping | shipping | ✅ | 104 |
| D04 | How do I change the email address on my profile? — Which team handles this ticket? | account | account | ✅ | 111 |
| D05 | I can't log in, it says invalid token after the update. — Which team handles this ticket? | technical | technical | ✅ | 99 |
| D06 | Please delete my account and all my data. — Which team handles this ticket? | account | account | ✅ | 98 |
| D07 | I was charged for a subscription I cancelled last week. — Which team handles this ticket? | billing | billing | ✅ | 100 |
| D08 | The tracking number hasn't updated in 9 days. — Which team handles this ticket? | shipping | shipping | ✅ | 100 |
| D09 | Absolutely love the new dashboard, great job team! — What is the sentiment? | positive | positive | ✅ | 89 |
| D10 | This is the third time the export fails. I'm done with this product. — What is the sentiment? | negative | negative | ✅ | 83 |
| D11 | The meeting is scheduled for 3 pm in room 4. — What is the sentiment? | neutral | neutral | ✅ | 82 |
| D12 | Thanks, the refund arrived quickly and support was kind. — What is the sentiment? | positive | positive | ✅ | 81 |
| D13 | Customer bought the item 10 days ago, unused, with receipt. Policy: returns accepted within 30 days for unused items with receipt. — Is the return allowed under | true | true (0.93) | ✅ | 102 |
| D14 | Customer bought the item 45 days ago, unused, with receipt. Policy: returns accepted within 30 days for unused items with receipt. — Is the return allowed under | false | false (0.11) | ✅ | 292 |
| D15 | Customer bought the item 5 days ago, it was used and has no receipt. Policy: returns accepted within 30 days for unused items with receipt. — Is the return allo | false | false (0.08) | ✅ | 105 |
| D16 | Email: 'Your account password expires today, click here to keep it: http://secure-login.example-verify.ru' — Is this email likely phishing? | true | true (0.90) | ✅ | 82 |
| D17 | Email from the team calendar: 'Reminder: sprint review tomorrow at 10:00 in the usual room.' — Is this email likely phishing? | false | false (0.13) | ✅ | 83 |
| D18 | Bug: users' saved documents are deleted when they log out; 40% of users affected; no backup. — How severe is this bug? | 3 | 2 (1.98) | ❌ | 100 |
| D19 | Bug: the footer text is 1px misaligned on the about page. — How severe is this bug? | 0 | 1 (1.00) | ❌ | 109 |
| D20 | Bug: exporting to PDF fails, but users can still print to PDF from the browser. — How severe is this bug? | 1 | 1 (1.34) | ✅ | 100 |

## OCR (glm-ocr:latest)

| # | Question | Expected | Answer | Pass | ms |
|---|---|---|---|---|---:|
| O01 | Invoice 4821 / Total due: $312.50 | Invoice 4821, 312.50 | Invoice 4821 Total due: $312.50 --- --- --- --- --- --- --- --- --- --- --- --- --- --- --- --- --- --- --- --- --- --- --- --- --- --- ---  | ✅ | 70322 |
| O02 | Receipt #00917 / Coffee 3.40 / Bagel 2.75 / TOTAL 6.15 | 00917, 3.40, 2.75, 6.15 | Receipt #00917 Coffee 3.40 Bagel 2.75 TOTAL 6.15 ```markdown Receipt #00917 Coffee 3.40 Bagel 2.75 TOTAL 6.15 ``` ``` ``` ``` ``` ``` ``` `` | ✅ | 3811 |
| O03 | ERROR 2026-10-31T14:05:22Z db: connection refused (port 5432) | connection refused, 5432 | ERROR 2026-10-31T14:05:22Z db: connection refused (port 5432) ```markdown ``` ERROR 2026-10-31T14:05:22Z db: connection refused (port 5432)  | ✅ | 3889 |
| O04 | function add(a, b) { /   return a + b; / } | function add(a, b), return a + b | ```javascript function add(a, b) { return a + b; } ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ` | ✅ | 1870 |
| O05 | Name      Qty   Price / Widget    4     9.99 / Gadget    2     24.50 | Widget, 9.99, Gadget, 24.50 | Name Qty Price Widget 4 9.99 Gadget 2 24.50 <table border="1"><tr><td>Name</td><td>Qty</td><td>Price</td></tr><tr><td>Widget</td><td>4</td>< | ✅ | 51778 |
| O06 | Ship to: Dana Levi / 12 Harbor Road, Apt 3B / Haifa 3303112 | Dana Levi, 12 Harbor Road, 3303112 | Ship to: Dana Levi 12 Harbor Road, Apt 3B Haifa 3303112 ```markdown ``` Ship to: Dana Levi 12 Harbor Road, Apt 3B Haifa 3303112 ``` ``` ```  | ✅ | 2074 |
| O07 | Contact: support@example.org / Phone: +1 (415) 555-0134 | support@example.org, 555-0134 | Contact: support@example.org Phone: +1 (415) 555-0134 ```markdown Contact: support@example.org Phone: +1 (415) 555-0134 ``` ``` ``` ``` ```  | ✅ | 1713 |
| O08 | https://github.com/bgauryy/FreeLlama/issues/42 | github.com/bgauryy/FreeLlama/issues/42 | https://github.com/bgauryy/FreeLlama/issues/42 ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ` | ✅ | 1629 |
| O09 | Meeting moved to Thursday, 14 November 2026 at 09:30 | 14 November 2026, 09:30 | Meeting moved to Thursday, 14 November 2026 at 09:30 ```markdown Meeting moved to Thursday, 14 November 2026 at 09:30 ``` ``` ``` Meeting mo | ✅ | 2242 |
| O10 | WARNING: disk usage at 91% on /dev/disk3s1 | 91%, /dev/disk3s1 | WARNING: disk usage at 91% on /dev/disk3s1 ```markdown ``` WARNING: disk usage at 91% on /dev/disk3s1 ``` ``` ``` ``` ``` ``` ``` ``` ``` `` | ✅ | 1483 |
| O11 | SKU: A7-3391-QX / Batch: 2026/10/B / Expires: 2027-03-31 | A7-3391-QX, 2027-03-31 | SKU: A7-3391-QX Batch: 2026/10/B Expires: 2027-03-31 ```markdown SKU: A7-3391-QX Batch: 2026/10/B Expires: 2027-03-31 ``` ``` ``` ``` ``` `` | ✅ | 2282 |
| O12 | $ npm install --save-dev vitest@3.2.4 | npm install, vitest@3.2.4 | $ npm install --save-dev vitest@3.2.4 ```bash ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` `` | ✅ | 1393 |
| O13 | Temperature: 21.5 C / Humidity: 48% / Pressure: 1013 hPa | 21.5, 48%, 1013 | Temperature: 21.5 C Humidity: 48% Pressure: 1013 hPa ```markdown Temperature: 21.5 C Humidity: 48% Pressure: 1013 hPa ``` ``` ``` ``` ``` `` | ✅ | 2002 |
| O14 | Order 77-1209 shipped via DHL / Tracking: JD014600006281923457 | 77-1209, JD014600006281923457 | Order 77-1209 shipped via DHL Tracking: JD014600006281923457 ```markdown Order 77-1209 shipped via DHL Tracking: JD014600006281923457 ``` `` | ✅ | 5615 |
| O15 | SELECT name, total FROM orders WHERE total > 100 ORDER BY total DESC; | SELECT name, total FROM orders, total > 100 | SELECT name, total FROM orders WHERE total > 100 ORDER BY total DESC; ```sql SELECT name, total FROM orders WHERE total > 100 ORDER BY total | ✅ | 14940 |
| O16 | Step 1: Unplug the router / Step 2: Wait 30 seconds / Step 3: Plug it back in | Unplug the router, 30 seconds, Plug it back in | Step 1: Unplug the router Step 2: Wait 30 seconds Step 3: Plug it back in ```markdown Step 1: Unplug the router Step 2: Wait 30 seconds Step | ✅ | 1966 |
| O17 | IBAN: DE89 3704 0044 0532 0130 00 | DE89 3704 0044 0532 0130 00 | IBAN: DE89 3704 0044 0532 0130 00 ```markdown ``` IBAN: DE89 3704 0044 0532 0130 00 ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ```  | ✅ | 2038 |
| O18 | Version 0.8.0 (build 2026.10.31-a1b2c3d) | 0.8.0, a1b2c3d | Version 0.8.0 (build 2026.10.31-a1b2c3d) ```markdown ``` Version 0.8.0 (build 2026.10.31-a1b2c3d) ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` `` | ✅ | 2593 |
| O19 | Q3 revenue: 1,240,000 / Q4 revenue: 1,515,500 / Growth: 22.2% | 1,240,000, 1,515,500, 22.2% | Q3 revenue: 1,240,000 Q4 revenue: 1,515,500 Growth: 22.2% ```markdown Q3 revenue: 1,240,000 Q4 revenue: 1,515,500 Growth: 22.2% ``` ``` ```  | ✅ | 2089 |
| O20 | def fib(n): /     return n if n < 2 else fib(n - 1) + fib(n - 2) | def fib(n), fib(n - 1) + fib(n - 2) | ```python def fib(n): return n if n < 2 else fib(n - 1) + fib(n - 2) ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` ``` | ✅ | 1696 |
| O21 | Patient ID: 558-21-0093 / Blood type: O+ / Allergies: penicillin | 558-21-0093, penicillin | Patient ID: 558-21-0093 Blood type: 0+ Allergies: penicillin ```markdown Patient ID: 558-21-0093 Blood type: 0+ Allergies: penicillin ``` `` | ✅ | 1866 |
| O22 | TODO(alice): remove the retry loop after 2026-12-01 | remove the retry loop, 2026-12-01 | TODO(alice): remove the retry loop after 2026-12-01 ```markdown ``` TODO(alice): remove the retry loop after 2026-12-01 ``` ``` ``` ``` ```  | ✅ | 1760 |
| O23 | Flight LY 008 TLV-JFK / Seat 34C, Gate B7, Boarding 23:15 | LY 008, 34C, B7, 23:15 | Flight LY 008 TLV-JFK Seat 34C, Gate B7, Boarding 23:15 ```markdown ``` Flight LY 008 TLV-JFK Seat 34C, Gate B7, Boarding 23:15 ``` ``` ```  | ✅ | 2651 |
| O24 | panic: runtime error: index out of range [5] with length 3 | index out of range, [5] with length 3 | panic: runtime error: index out of range [5] with length 3 ```markdown ``` panic: runtime error: index out of range [5] with length 3 ``` `` | ✅ | 5450 |
| O25 | Password must contain at least 12 characters, 1 digit and 1 symbol. | at least 12 characters, 1 digit | Password must contain at least 12 characters, 1 digit and 1 symbol. ```markdown Password must contain at least 12 characters, 1 digit and 1  | ✅ | 1593 |
| O26 | curl -s http://127.0.0.1:11435/_freellama/v1/ready | 127.0.0.1:11435, /_freellama/v1/ready | curl -s http://127.0.0.1:11435/_freellama/v1/ready ```bash curl -s http://127.0.0.1:11435/_freellama/v1/ready ``` ``` ``` ``` ``` ``` ``` `` | ✅ | 1884 |
| O27 | Monday    09:00-17:00 / Saturday  10:00-14:00 / Sunday    closed | 09:00-17:00, 10:00-14:00, closed | Monday 09:00-17:00 Saturday 10:00-14:00 Sunday closed ```markdown Monday 09:00-17:00 Saturday 10:00-14:00 Sunday closed ``` ``` ``` ``` ```  | ✅ | 1983 |
| O28 | git commit -m "fix: handle empty prompt before routing" | git commit -m, handle empty prompt before routing | git commit -m "fix: handle empty prompt before routing" ```bash git commit -m "fix: handle empty prompt before routing" ``` ``` ``` ``` ```  | ✅ | 28823 |
| O29 | Total memory 34359738368 bytes; available 6480527360 bytes | 34359738368, 6480527360 | Total memory 34359738368 bytes; available 6480527360 bytes ```markdown Total memory 34359738368 bytes; available 6480527360 bytes ``` ``` `` | ✅ | 2455 |
| O30 | Licensed under the Apache License, Version 2.0 (the "License"); / you may not use this file except in compliance with the License. | Apache License, Version 2.0, except in compliance with the License | Licensed under the Apache License, Version 2.0 (the "License"); you may not use this file except in compliance with the License. ```markdown | ✅ | 2014 |

## Small coding tasks (gpt-oss:20b)

| # | Question | Expected | Answer | Pass | ms |
|---|---|---|---|---|---:|
| C01 | What does this JavaScript print? console.log([1, 2, 3].map(x => x * 2).join('-')) | 2-4-6 | held: held_insufficient_capacity | held |  |
| C02 | What does this Python print? print(sum(range(5))) | 10 | held: held_insufficient_capacity | held |  |
| C03 | What does this JavaScript print? console.log(typeof null) | object | held: held_insufficient_capacity | held |  |
| C04 | What does this Python print? print('abc'[::-1]) | cba | held: held_insufficient_capacity | held |  |
| C05 | What does this JavaScript print? console.log(0.1 + 0.2 === 0.3) | false | held: held_insufficient_capacity | held |  |
| C06 | What does this Rust expression evaluate to? (1..=4).fold(0, \|a, b\| a + b) | 10 | held: held_insufficient_capacity | held |  |
| C07 | What does this Python print? print(len({1, 2, 2, 3, 3, 3})) | 3 | held: held_insufficient_capacity | held |  |
| C08 | What does this JavaScript print? console.log([10, 1, 3].sort()) | \[?\s*1\s*,\s*10\s*,\s*3\s*\]? | held: held_insufficient_capacity | held |  |
| C09 | What HTTP status code means 'Too Many Requests'? Answer with the number. | 429 | held: held_insufficient_capacity | held |  |
| C10 | What does this shell command print? echo $((7 * 6)) | 42 | held: held_insufficient_capacity | held |  |
| C11 | This function should return the largest number but returns the smallest. Reply with the corrected return line only. function max(a) { return a.reduce((m, x) =>  | x\s*>\s*m\s*\?\s*x\s*:\s*m\|Math\.max | held: held_insufficient_capacity | held |  |
| C12 | Fix the off-by-one error. Reply with the corrected loop header only. for (let i = 0; i <= arr.length; i++) total += arr[i]; | i\s*<\s*arr\.length | held: held_insufficient_capacity | held |  |
| C13 | Write a JavaScript regular expression literal that matches a 4-digit year like 2026 and nothing longer. Reply with the regex only. | \\d\{4\}\|\[0-9\]\{4\} | held: held_insufficient_capacity | held |  |
| C14 | Which git command shows the commits that are on branch feature but not on main? Reply with the command only. | git log\s+main\.\.feature | held: held_insufficient_capacity | held |  |
| C15 | Name the Rust trait you implement so a type can be printed with {}. Reply with the trait name only. | Display | held: held_insufficient_capacity | held |  |
| C16 | In SQL, which clause filters groups after GROUP BY? Reply with the keyword only. | HAVING | held: held_insufficient_capacity | held |  |
| C17 | What is the time complexity of binary search? Reply in big-O notation only. | O\(\s*log\s*n\s*\) | held: held_insufficient_capacity | held |  |
| C18 | This Python returns None instead of the list. Reply with the corrected line only. def doubled(xs):     result = [x * 2 for x in xs] | return | held: held_insufficient_capacity | held |  |
| C19 | Convert to an arrow function, reply with code only: function sq(x) { return x * x; } | =>\s*x\s*\*\s*x | held: held_insufficient_capacity | held |  |
| C20 | What does this TypeScript type evaluate to? type T = Exclude<'a' \| 'b' \| 'c', 'b'> | ['"]a['"]\s*\\|\s*['"]c['"] | held: held_insufficient_capacity | held |  |
| C21 | Which npm command installs exactly the versions in package-lock.json, for CI? Reply with the command only. | npm ci | held: held_insufficient_capacity | held |  |
| C22 | In Python, which built-in returns pairs of (index, value) while iterating a list? Reply with the name only. | enumerate | held: held_insufficient_capacity | held |  |
| C23 | What does this JavaScript print? console.log([...'hello'].filter(c => c === 'l').length) | 2 | held: held_insufficient_capacity | held |  |
| C24 | Write a one-line shell command that counts the lines of file.txt. Reply with the command only. | wc\s+-l | held: held_insufficient_capacity | held |  |
| C25 | Is this JSON valid? Reply yes or no. {"a": 1, "b": [1, 2,]} | \bno\b | held: held_insufficient_capacity | held |  |

## Small research agent: delegate_research (gpt-oss:20b)

| # | Question | Expected | Answer | Pass | ms |
|---|---|---|---|---|---:|
| A01 | In packages/rust-core/src/platform/tasks_api.rs, what is the value of MAX_BATCH_TASKS? | 64 | held: held_insufficient_capacity | held |  |
| A02 | Which file in packages/rust-core/src/platform defines the function unload_resident_model? | execution/mod.rs | held: held_insufficient_capacity | held |  |
| A03 | In packages/rust-core/src/platform/jobs.rs, how many seconds is RESULT_TTL? | 600 | held: held_insufficient_capacity | held |  |
| A04 | In packages/cli/src/cpu_backend.rs, what port does --spawn-cpu-backend use by default? Check packages/cli/src/args.rs. | 11436 | held: held_insufficient_capacity | held |  |
| A05 | In packages/mcp/src/tools/run-task.ts, what is the value of EMBEDDINGS_PER_JOB? | 32 | held: held_insufficient_capacity | held |  |

