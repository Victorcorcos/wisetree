#!/usr/bin/env node
// Postinstall shim: locate the platform-specific binary that npm pulled in
// via `optionalDependencies` and surface it as `bin/wisetree` in the main
// package. The `bin/wisetree` JS shim then `execvp`s into it at runtime.
//
// This keeps npm's resolution honest (only the matching platform package is
// downloaded) while giving us a single canonical bin path to reference from
// `package.json`.

const fs = require("node:fs");
const path = require("node:path");
const { platform, arch } = process;

const targets = {
  "darwin arm64": { pkg: "wisetree-darwin-arm64", bin: "wisetree" },
  "darwin x64": { pkg: "wisetree-darwin-x64", bin: "wisetree" },
  "linux x64": { pkg: "wisetree-linux-x64-gnu", bin: "wisetree" },
  "linux arm64": { pkg: "wisetree-linux-arm64-gnu", bin: "wisetree" },
  "win32 x64": { pkg: "wisetree-win32-x64-msvc", bin: "wisetree.exe" },
};

const key = `${platform} ${arch}`;
const target = targets[key];
if (!target) {
  console.error(
    `wisetree: no prebuilt binary for ${key}. Supported: ${Object.keys(targets).join(", ")}.`
  );
  console.error(
    "Install from source instead: `cargo install wisetree`."
  );
  process.exit(1);
}

let resolvedBinary;
try {
  // require.resolve walks node_modules — works whether the platform package
  // lives next to us (typical) or hoisted (workspaces).
  const pkgJsonPath = require.resolve(`${target.pkg}/package.json`);
  resolvedBinary = path.join(path.dirname(pkgJsonPath), "bin", target.bin);
} catch (err) {
  console.error(
    `wisetree: optional dependency '${target.pkg}' was not installed. ` +
      "This usually means npm filtered it out (e.g. --no-optional) or the " +
      "platform package failed to download."
  );
  process.exit(1);
}

if (!fs.existsSync(resolvedBinary)) {
  console.error(
    `wisetree: expected binary at ${resolvedBinary} but it was missing.`
  );
  process.exit(1);
}

if (process.platform !== "win32") {
  try {
    fs.chmodSync(resolvedBinary, 0o755);
  } catch (err) {
    // Best-effort: continue even if chmod fails (e.g. on a read-only FS).
  }
}
