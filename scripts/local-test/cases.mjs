// The 100 LOCAL_TEST questions with their known answers. Kept apart from the runner so the set can
// grow without touching how it runs.

const choice = (instructions, criteria) => ({ type: "choice", instructions, criteria });
const noul = (instructions) => ({ type: "noul", instructions });
const score = (instructions, criteria) => ({ type: "score", instructions, criteria });
const TEAMS = { billing: "Charges, invoices, refunds of payments", technical: "App errors, crashes, bugs, login failures", shipping: "Delivery, tracking, lost or late parcels", account: "Profile, email change, account deletion, settings" };
const SENTIMENT = { positive: "Happy, satisfied, praising", negative: "Unhappy, angry, complaining", neutral: "Factual, no clear feeling" };
const SEVERITY = ["Cosmetic: no impact", "Minor: workaround exists", "Major: a feature is broken", "Critical: data loss, outage or security breach"];

export const CASES = {
  // ---------------- OCR: glm-ocr reads a rendered page; every expected phrase must appear ----------------
  ocr: [
    { id: "O01", lines: ["Invoice 4821", "Total due: $312.50"], expect: ["Invoice 4821", "312.50"] },
    { id: "O02", lines: ["Receipt #00917", "Coffee 3.40", "Bagel 2.75", "TOTAL 6.15"], expect: ["00917", "3.40", "2.75", "6.15"] },
    { id: "O03", lines: ["ERROR 2026-10-31T14:05:22Z db: connection refused (port 5432)"], expect: ["connection refused", "5432"] },
    { id: "O04", lines: ["function add(a, b) {", "  return a + b;", "}"], expect: ["function add(a, b)", "return a + b"] },
    { id: "O05", lines: ["Name      Qty   Price", "Widget    4     9.99", "Gadget    2     24.50"], expect: ["Widget", "9.99", "Gadget", "24.50"] },
    { id: "O06", lines: ["Ship to: Dana Levi", "12 Harbor Road, Apt 3B", "Haifa 3303112"], expect: ["Dana Levi", "12 Harbor Road", "3303112"] },
    { id: "O07", lines: ["Contact: support@example.org", "Phone: +1 (415) 555-0134"], expect: ["support@example.org", "555-0134"] },
    { id: "O08", lines: ["https://github.com/bgauryy/FreeLlama/issues/42"], expect: ["github.com/bgauryy/FreeLlama/issues/42"] },
    { id: "O09", lines: ["Meeting moved to Thursday, 14 November 2026 at 09:30"], expect: ["14 November 2026", "09:30"] },
    { id: "O10", lines: ["WARNING: disk usage at 91% on /dev/disk3s1"], expect: ["91%", "/dev/disk3s1"] },
    { id: "O11", lines: ["SKU: A7-3391-QX", "Batch: 2026/10/B", "Expires: 2027-03-31"], expect: ["A7-3391-QX", "2027-03-31"] },
    { id: "O12", lines: ["$ npm install --save-dev vitest@3.2.4"], expect: ["npm install", "vitest@3.2.4"] },
    { id: "O13", lines: ["Temperature: 21.5 C", "Humidity: 48%", "Pressure: 1013 hPa"], expect: ["21.5", "48%", "1013"] },
    { id: "O14", lines: ["Order 77-1209 shipped via DHL", "Tracking: JD014600006281923457"], expect: ["77-1209", "JD014600006281923457"] },
    { id: "O15", lines: ["SELECT name, total FROM orders WHERE total > 100 ORDER BY total DESC;"], expect: ["SELECT name, total FROM orders", "total > 100"] },
    { id: "O16", lines: ["Step 1: Unplug the router", "Step 2: Wait 30 seconds", "Step 3: Plug it back in"], expect: ["Unplug the router", "30 seconds", "Plug it back in"] },
    { id: "O17", lines: ["IBAN: DE89 3704 0044 0532 0130 00"], expect: ["DE89 3704 0044 0532 0130 00"] },
    { id: "O18", lines: ["Version 0.8.0 (build 2026.10.31-a1b2c3d)"], expect: ["0.8.0", "a1b2c3d"] },
    { id: "O19", lines: ["Q3 revenue: 1,240,000", "Q4 revenue: 1,515,500", "Growth: 22.2%"], expect: ["1,240,000", "1,515,500", "22.2%"] },
    { id: "O20", lines: ["def fib(n):", "    return n if n < 2 else fib(n - 1) + fib(n - 2)"], expect: ["def fib(n)", "fib(n - 1) + fib(n - 2)"] },
    { id: "O21", lines: ["Patient ID: 558-21-0093", "Blood type: O+", "Allergies: penicillin"], expect: ["558-21-0093", "penicillin"] },
    { id: "O22", lines: ["TODO(alice): remove the retry loop after 2026-12-01"], expect: ["remove the retry loop", "2026-12-01"] },
    { id: "O23", lines: ["Flight LY 008 TLV-JFK", "Seat 34C, Gate B7, Boarding 23:15"], expect: ["LY 008", "34C", "B7", "23:15"] },
    { id: "O24", lines: ["panic: runtime error: index out of range [5] with length 3"], expect: ["index out of range", "[5] with length 3"] },
    { id: "O25", lines: ["Password must contain at least 12 characters, 1 digit and 1 symbol."], expect: ["at least 12 characters", "1 digit"] },
    { id: "O26", lines: ["curl -s http://127.0.0.1:11435/_freellama/v1/ready"], expect: ["127.0.0.1:11435", "/_freellama/v1/ready"] },
    { id: "O27", lines: ["Monday    09:00-17:00", "Saturday  10:00-14:00", "Sunday    closed"], expect: ["09:00-17:00", "10:00-14:00", "closed"] },
    { id: "O28", lines: ["git commit -m \"fix: handle empty prompt before routing\""], expect: ["git commit -m", "handle empty prompt before routing"] },
    { id: "O29", lines: ["Total memory 34359738368 bytes; available 6480527360 bytes"], expect: ["34359738368", "6480527360"] },
    { id: "O30", lines: ["Licensed under the Apache License, Version 2.0 (the \"License\");", "you may not use this file except in compliance with the License."], expect: ["Apache License, Version 2.0", "except in compliance with the License"] },
  ],

  // ---------------- Classification with the decision model (System One API) ----------------
  decide: [
    { id: "D01", state: "Why did my credit card get billed two times this month?", question: choice("Which team handles this ticket?", TEAMS), expect: "billing" },
    { id: "D02", state: "The app crashes every time I open the settings screen.", question: choice("Which team handles this ticket?", TEAMS), expect: "technical" },
    { id: "D03", state: "My parcel shows delivered but nothing arrived.", question: choice("Which team handles this ticket?", TEAMS), expect: "shipping" },
    { id: "D04", state: "How do I change the email address on my profile?", question: choice("Which team handles this ticket?", TEAMS), expect: "account" },
    { id: "D05", state: "I can't log in, it says invalid token after the update.", question: choice("Which team handles this ticket?", TEAMS), expect: "technical" },
    { id: "D06", state: "Please delete my account and all my data.", question: choice("Which team handles this ticket?", TEAMS), expect: "account" },
    { id: "D07", state: "I was charged for a subscription I cancelled last week.", question: choice("Which team handles this ticket?", TEAMS), expect: "billing" },
    { id: "D08", state: "The tracking number hasn't updated in 9 days.", question: choice("Which team handles this ticket?", TEAMS), expect: "shipping" },
    { id: "D09", state: "Absolutely love the new dashboard, great job team!", question: choice("What is the sentiment?", SENTIMENT), expect: "positive" },
    { id: "D10", state: "This is the third time the export fails. I'm done with this product.", question: choice("What is the sentiment?", SENTIMENT), expect: "negative" },
    { id: "D11", state: "The meeting is scheduled for 3 pm in room 4.", question: choice("What is the sentiment?", SENTIMENT), expect: "neutral" },
    { id: "D12", state: "Thanks, the refund arrived quickly and support was kind.", question: choice("What is the sentiment?", SENTIMENT), expect: "positive" },
    { id: "D13", state: "Customer bought the item 10 days ago, unused, with receipt. Policy: returns accepted within 30 days for unused items with receipt.", question: noul("Is the return allowed under the policy?"), expect: "true" },
    { id: "D14", state: "Customer bought the item 45 days ago, unused, with receipt. Policy: returns accepted within 30 days for unused items with receipt.", question: noul("Is the return allowed under the policy?"), expect: "false" },
    { id: "D15", state: "Customer bought the item 5 days ago, it was used and has no receipt. Policy: returns accepted within 30 days for unused items with receipt.", question: noul("Is the return allowed under the policy?"), expect: "false" },
    { id: "D16", state: "Email: 'Your account password expires today, click here to keep it: http://secure-login.example-verify.ru'", question: noul("Is this email likely phishing?"), expect: "true" },
    { id: "D17", state: "Email from the team calendar: 'Reminder: sprint review tomorrow at 10:00 in the usual room.'", question: noul("Is this email likely phishing?"), expect: "false" },
    { id: "D18", state: "Bug: users' saved documents are deleted when they log out; 40% of users affected; no backup.", question: score("How severe is this bug?", SEVERITY), expect: 3 },
    { id: "D19", state: "Bug: the footer text is 1px misaligned on the about page.", question: score("How severe is this bug?", SEVERITY), expect: 0 },
    { id: "D20", state: "Bug: exporting to PDF fails, but users can still print to PDF from the browser.", question: score("How severe is this bug?", SEVERITY), expect: 1 },
  ],

  // ---------------- Classification with embeddings: nearest per-class centroid ----------------
  embedTraining: {
    bug: ["The app crashes when I upload a photo", "Clicking save throws a null pointer error", "Login fails with error 500 since yesterday"],
    feature: ["Please add dark mode to the dashboard", "It would be great to export reports as CSV", "Can you support single sign-on with Google?"],
    question: ["How do I reset my password?", "Where can I find my invoices?", "What is the difference between the Pro and Team plans?"],
    docs: ["The README install steps are outdated", "The API reference is missing the pagination parameter", "Typo in the getting started guide, step 3"],
  },
  embed: [
    { id: "E01", text: "Uploading a PDF freezes the browser tab", expect: "bug" },
    { id: "E02", text: "Add keyboard shortcuts for switching projects", expect: "feature" },
    { id: "E03", text: "How can I invite a teammate to my workspace?", expect: "question" },
    { id: "E04", text: "The docs page for webhooks shows the wrong header name", expect: "docs" },
    { id: "E05", text: "Sync stops with error code 0x80070005 after the update", expect: "bug" },
    { id: "E06", text: "Would love a Slack integration for notifications", expect: "feature" },
    { id: "E07", text: "Is there a limit on how many files I can store?", expect: "question" },
    { id: "E08", text: "The tutorial links to a page that no longer exists", expect: "docs" },
    { id: "E09", text: "Search returns duplicate results for every query", expect: "bug" },
    { id: "E10", text: "Please support exporting charts as PNG images", expect: "feature" },
    { id: "E11", text: "Which payment methods do you accept?", expect: "question" },
    { id: "E12", text: "The CLI reference does not mention the --verbose flag", expect: "docs" },
    { id: "E13", text: "Notifications arrive twice on Android", expect: "bug" },
    { id: "E14", text: "Add a calendar view for tasks", expect: "feature" },
    { id: "E15", text: "Can I change my username after signing up?", expect: "question" },
    { id: "E16", text: "Code sample in the Python quickstart has a syntax error", expect: "docs" },
    { id: "E17", text: "The progress bar stays at 99% forever", expect: "bug" },
    { id: "E18", text: "Offer a self-hosted version for enterprises", expect: "feature" },
    { id: "E19", text: "Do you have a student discount?", expect: "question" },
    { id: "E20", text: "Changelog entry for 2.1 is missing from the docs site", expect: "docs" },
  ],

  // ---------------- Small coding tasks: a short answer with a checkable fact ----------------
  code: [
    { id: "C01", prompt: "What does this JavaScript print?\nconsole.log([1, 2, 3].map(x => x * 2).join('-'))", expect: "2-4-6" },
    { id: "C02", prompt: "What does this Python print?\nprint(sum(range(5)))", expect: "10" },
    { id: "C03", prompt: "What does this JavaScript print?\nconsole.log(typeof null)", expect: "object" },
    { id: "C04", prompt: "What does this Python print?\nprint('abc'[::-1])", expect: "cba" },
    { id: "C05", prompt: "What does this JavaScript print?\nconsole.log(0.1 + 0.2 === 0.3)", expect: "false" },
    { id: "C06", prompt: "What does this Rust expression evaluate to?\n(1..=4).fold(0, |a, b| a + b)", expect: "10" },
    { id: "C07", prompt: "What does this Python print?\nprint(len({1, 2, 2, 3, 3, 3}))", expect: "3" },
    { id: "C08", prompt: "What does this JavaScript print?\nconsole.log([10, 1, 3].sort())", expect: /\[?\s*1\s*,\s*10\s*,\s*3\s*\]?/ },
    { id: "C09", prompt: "What HTTP status code means 'Too Many Requests'? Answer with the number.", expect: "429" },
    { id: "C10", prompt: "What does this shell command print?\necho $((7 * 6))", expect: "42" },
    { id: "C11", prompt: "This function should return the largest number but returns the smallest. Reply with the corrected return line only.\nfunction max(a) { return a.reduce((m, x) => x < m ? x : m); }", expect: /x\s*>\s*m\s*\?\s*x\s*:\s*m|Math\.max/ },
    { id: "C12", prompt: "Fix the off-by-one error. Reply with the corrected loop header only.\nfor (let i = 0; i <= arr.length; i++) total += arr[i];", expect: /i\s*<\s*arr\.length/ },
    { id: "C13", prompt: "Write a JavaScript regular expression literal that matches a 4-digit year like 2026 and nothing longer. Reply with the regex only.", expect: /\\d\{4\}|\[0-9\]\{4\}/ },
    { id: "C14", prompt: "Which git command shows the commits that are on branch feature but not on main? Reply with the command only.", expect: /git log\s+main\.\.feature/ },
    { id: "C15", prompt: "Name the Rust trait you implement so a type can be printed with {}. Reply with the trait name only.", expect: "Display" },
    { id: "C16", prompt: "In SQL, which clause filters groups after GROUP BY? Reply with the keyword only.", expect: "HAVING" },
    { id: "C17", prompt: "What is the time complexity of binary search? Reply in big-O notation only.", expect: /O\(\s*log\s*n\s*\)/i },
    { id: "C18", prompt: "This Python returns None instead of the list. Reply with the corrected line only.\ndef doubled(xs):\n    result = [x * 2 for x in xs]", expect: "return" },
    { id: "C19", prompt: "Convert to an arrow function, reply with code only: function sq(x) { return x * x; }", expect: /=>\s*x\s*\*\s*x/ },
    { id: "C20", prompt: "What does this TypeScript type evaluate to? type T = Exclude<'a' | 'b' | 'c', 'b'>", expect: /['"]a['"]\s*\|\s*['"]c['"]/ },
    { id: "C21", prompt: "Which npm command installs exactly the versions in package-lock.json, for CI? Reply with the command only.", expect: "npm ci" },
    { id: "C22", prompt: "In Python, which built-in returns pairs of (index, value) while iterating a list? Reply with the name only.", expect: "enumerate" },
    { id: "C23", prompt: "What does this JavaScript print?\nconsole.log([...'hello'].filter(c => c === 'l').length)", expect: "2" },
    { id: "C24", prompt: "Write a one-line shell command that counts the lines of file.txt. Reply with the command only.", expect: /wc\s+-l/ },
    { id: "C25", prompt: "Is this JSON valid? Reply yes or no.\n{\"a\": 1, \"b\": [1, 2,]}", expect: /\bno\b/i },
  ],

  // ---------------- Small research agent: delegate_research over this repository ----------------
  agent: [
    { id: "A01", prompt: "In packages/rust-core/src/platform/tasks_api.rs, what is the value of MAX_BATCH_TASKS?", expect: "64" },
    { id: "A02", prompt: "Which file in packages/rust-core/src/platform defines the function unload_resident_model?", expect: "execution/mod.rs" },
    { id: "A03", prompt: "In packages/rust-core/src/platform/jobs.rs, how many seconds is RESULT_TTL?", expect: "600" },
    { id: "A04", prompt: "In packages/cli/src/cpu_backend.rs, what port does --spawn-cpu-backend use by default? Check packages/cli/src/args.rs.", expect: "11436" },
    { id: "A05", prompt: "In packages/mcp/src/tools/run-task.ts, what is the value of EMBEDDINGS_PER_JOB?", expect: "32" },
  ],
};

const count = CASES.ocr.length + CASES.decide.length + CASES.embed.length + CASES.code.length + CASES.agent.length;
if (count !== 100) throw new Error(`LOCAL_TEST must have 100 questions, has ${count}`);
