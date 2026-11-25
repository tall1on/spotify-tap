import { spawn } from "node:child_process";
import { copyFile, mkdir, rename, rm } from "node:fs/promises";
import { dirname, join } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const destination = join(root, "dist", "spotify_tap.dll");

// The DLL is loaded inside Spotify, so a running Spotify locks its copy.
// Loaders should inject a private copy (for example from LOCALAPPDATA) so this
// build output stays free to replace.
await rm(destination, { force: true });
const cargo = spawn(
  "cargo",
  ["build", "--release", "--locked", "--message-format=json-render-diagnostics"],
  {
    cwd: root,
    stdio: ["inherit", "pipe", "inherit"],
  },
);
let artifact;
const output = createInterface({ input: cargo.stdout });
output.on("line", (line) => {
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    console.log(line);
    return;
  }
  if (
    message.reason === "compiler-artifact" &&
    message.target.name === "spotify_tap" &&
    message.target.crate_types.includes("cdylib")
  ) {
    artifact = message.filenames.find((path) => path.endsWith(".dll"));
  }
});

const exitCode = await new Promise((resolve, reject) => {
  cargo.on("error", reject);
  cargo.on("close", (code) => resolve(code ?? 1));
});
if (exitCode !== 0) process.exit(exitCode);
if (!artifact) throw new Error("Cargo did not produce spotify_tap.dll");
await mkdir(dirname(destination), { recursive: true });
const temporary = `${destination}.tmp`;
await copyFile(artifact, temporary);
await rename(temporary, destination);
console.log(`Built ${destination}`);
