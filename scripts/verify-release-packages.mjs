#!/usr/bin/env node
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";

for (const directory of ["packages/cli", "packages/mcp"]) {
  const output = execFileSync("npm", ["pack", "--dry-run", "--json", "--ignore-scripts"], {
    cwd: directory,
    encoding: "utf8",
  });
  const report = JSON.parse(output)[0];
  if (!report?.files?.length) throw new Error(`${directory}: npm pack contains no files`);
  if (!report.files.some((file) => file.path === "package.json")) {
    throw new Error(`${directory}: package.json missing from npm pack`);
  }
  console.log(`${directory}: ${report.files.length} files, ${report.size} packed bytes`);
}

const root = JSON.parse(readFileSync("package.json", "utf8"));
if (!root.scripts["release:verify"]) throw new Error("release:verify script is not registered");
