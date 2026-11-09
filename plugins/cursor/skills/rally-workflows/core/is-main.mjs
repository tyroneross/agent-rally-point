// SPDX-License-Identifier: Apache-2.0
import { realpathSync } from "node:fs";
import { fileURLToPath } from "node:url";

// npm command links and paths containing spaces must identify the same entry file.
export function isMain(moduleUrl, entryPath = process.argv[1]) {
  if (!entryPath) return false;
  try {
    return fileURLToPath(moduleUrl) === realpathSync(entryPath);
  } catch {
    return false;
  }
}
