Octocode's GitHub tools research public and private repositories without cloning them: find a repo, its code, its history, and its packages.
Call: octocode <tool> '{"queries":[{...}]}'  (default text; add --json for JSON)
Result: findings plus status/notes; --json gives results[].data. Exits: 0 success, 1 empty, 2 invalid input, 3 not found, 4 auth required (octocode auth login), 5 execution error, 7 rate limited, 130 interrupted.
More: run responsePagination.next, then every next.* continuation unchanged; report isPartial, terminalLimit and provider/index limits rather than assuming completeness.
Checked with local CLI 20.2.0; use octocode <tool> --help before its first call, or schema <tool> for fields.
ghSearchRepo: find a repository when you do not know its owner or name.
  {"operation":"search","keywords":["local llm router"],"filters":{"language":"Rust"}}
  {"operation":"get","owner":"ollama","repo":"ollama"}  one repo: description, stars, default branch
artifactSearch: find a package's repository and versions.
  {"operation":"resolve","ecosystem":"npm","packageName":"zod"}  also pypi, maven
ghStructure: see a repo's layout; operation "refs" lists branches, "languages" its languages.
  {"operation":"tree","owner":"ollama","repo":"ollama","path":"server","maxDepth":2}
ghSearchCode: find a word or name inside a repo when you do not know the file.
  {"owner":"ollama","repo":"ollama","keywords":["SystemOneRequest"]}  returns paths, lines and a commitSha
ghGetFileContent: read a file you found; ask for the least that answers.
  {"owner":"ollama","repo":"ollama","path":"api/types.go","view":"symbols"}  outline
  {"owner":"ollama","repo":"ollama","path":"api/types.go","ranges":[{"startLine":600,"endLine":640}]}
  {"owner":"ollama","repo":"ollama","path":"api/types.go","matchString":["type SystemOneRequest"]}
ghSearchHistory: find why something changed: pull requests, issues, commits.
  {"operation":"pullRequest","owner":"ollama","repo":"ollama","keywords":["systemone"]}
  {"operation":"commit","owner":"ollama","repo":"ollama","keywords":["decision"]}  or operation "issue" with "filters":{"state":"open"}
ghGetHistoryItem: read one pull request, or issue, you found.
  {"operation":"pullRequest","owner":"ollama","repo":"ollama","number":18606,"sections":["body","files"]}

Work smart:
1. Narrow: repo, then search, then read only the ranges you need. Never read whole files first.
2. Reuse what a result returns: its path, commitSha (as ref) and the next step it suggests.
3. Search words are exact code words, not sentences: one to three identifiers beat a question.
4. Nothing found proves little: try another word or check the repo name before saying it does not exist.
5. Answer with owner/repo, path:line or PR number as evidence, and stop as soon as the question is answered.
