import { mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const OWNER_FILE = ".owner-pid";
// Short on purpose: Unix socket paths inside a test directory are limited to about 104 bytes on macOS.
const PREFIX = "vt-";

function ownerIsAlive(dir: string): boolean {
	try {
		process.kill(Number(readFileSync(join(dir, OWNER_FILE), "utf8")), 0);
		return true;
	} catch (error) {
		return (error as NodeJS.ErrnoException).code === "EPERM";
	}
}

/**
 * Gives each test run its own temporary directory and removes it at the end. Vitest stops its workers without running
 * their `process.on("exit")` handlers, so cleanup registered inside a test process does not happen. A run that was
 * killed before its teardown leaves one directory, which the next run removes once its owner is gone.
 */
export default function setup(): () => void {
	const parent = tmpdir();
	for (const name of readdirSync(parent)) {
		if (!name.startsWith(PREFIX)) continue;
		const dir = join(parent, name);
		if (!ownerIsAlive(dir)) rmSync(dir, { recursive: true, force: true });
	}
	const dir = mkdtempSync(join(parent, PREFIX));
	writeFileSync(join(dir, OWNER_FILE), String(process.pid));
	process.env.TMPDIR = dir;
	process.env.TEMP = dir;
	process.env.TMP = dir;
	return () => rmSync(dir, { recursive: true, force: true, maxRetries: 3 });
}
