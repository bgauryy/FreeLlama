Octocode is a read-only code research CLI: it finds code in a directory so you read a few exact lines, not whole files.
Call: octocode <tool> '{"queries":[{...}]}'  (1-5 queries per call; paths relative to the working directory)
Result: lines of findings, then "#" notes. Exit 0 found, 1 nothing found, 2 bad input (the message names the field), 3 path not found.
More: copy a printed {"cursor":"..."} as the whole input. No cursor means the result is complete.

structureSearch: see the layout or list files, when you do not know where to look.
  {"operation":"tree","path":".","maxDepth":2}
  {"operation":"files","path":".","include":["**/*.rs"],"view":"paths"}
localSearch: find a word, name or error text, when you do not know the file.
  {"path":".","matchString":["memory_holders"],"view":"files"}  which files
  {"path":".","matchString":["memory_holders","holders"]}  matching lines with line numbers; several words mean any of them
localFetch: read a file an earlier result named (src/a.rs below is a placeholder); ask for the least that answers.
  {"path":"src/a.rs","view":"symbols"}  outline with line numbers
  {"path":"src/a.rs","ranges":[{"startLine":40,"endLine":90}]}
  {"path":"src/a.rs","matchString":["fn run"],"contextLines":{"before":2,"after":15}}
astSearch: find code by shape, not by text. Use it to find where something is declared.
  {"operation":"symbols","path":".","symbolFilter":{"terms":["run"],"mode":"exact"}}  declarations named run
  {"operation":"match","path":".","rule":{"pattern":"eval($X)"}}  every call of this shape
lspSearch: who defines, uses or calls a symbol; exact where text search only guesses. Take path and line from an earlier result, never guess them.
  {"path":"src/a.rs","symbolName":"run","line":35}  its definition
  {"operation":"references","path":"src/a.rs","symbolName":"run","line":35,"view":"files"}  where it is used
  {"operation":"callers","path":"src/a.rs","symbolName":"run","line":35}  also callees, hover, documentSymbols

Work smart:
1. Narrow before reading: list files or matching lines first, then read only those ranges.
2. Try several words at once: put synonyms in one matchString, or several queries in one call.
3. Build and dependency folders are skipped already; add "testCode":"exclude" to skip tests.
4. Nothing found proves little: widen the words or the path before saying something does not exist. If lspSearch is empty, use localSearch.
5. Answer with file:line evidence, and stop as soon as the question is answered.
