// The prompt an agent would write itself, with no FreeLlama workflow behind it: one line naming
// the task and, where a workflow needs one, its answer format. `workflows.mjs --variant` sends these
// instead of the shipped prompts, so the difference measures what the shipped prompts add.
export default {
  "summarize-text": "Summarize the text in one or two sentences.",
  "summarize-log": "Summarize this log: what failed and why.",
  "summarize-diff": "Summarize what this diff changes.",
  "extract-json": "Extract the requested fields from the text. Answer with JSON only.",
  "explain-code": "Explain what this code does.",
  "review-code": "Review this code and list any bugs.",
  "fix-bug": "Fix the bug. Answer with a SEARCH/REPLACE block: <<<<<<< SEARCH, the exact current lines, =======, the new lines, >>>>>>> REPLACE.",
  "change-code": "Make the requested change. Answer with a SEARCH/REPLACE block: <<<<<<< SEARCH, the exact current lines, =======, the new lines, >>>>>>> REPLACE.",
  "write-tests": "Write unit tests for this code.",
};
