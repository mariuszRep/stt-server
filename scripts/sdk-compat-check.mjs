#!/usr/bin/env node
/**
 * Cross-repository compatibility gate for `split-current-functionality-into-three-components`
 * (Scope 7 / AC8-9): proves a real, published `@open-vibe-ai/stt-sdk` version and a real,
 * released `stt-server` faster-whisper runtime binary actually work together, using only
 * installed/downloaded artifacts -- no sibling source checkout, no in-repo build of either side.
 *
 * Expects a faster-whisper runtime already running and healthy at `COMPAT_BASE_URL`
 * (VOICE_TYPER_AUTH_TOKEN = COMPAT_TOKEN), and `@open-vibe-ai/stt-sdk` installed in the
 * current working directory's node_modules at the exact pinned version being tested.
 *
 * Checks, in order (each a real network round-trip, none stubbed):
 *   1. Authenticated GET /health
 *   2. Authenticated GET /v1/config
 *   3. Raw multipart POST /v1/audio/transcriptions (the literal OpenDora wire contract:
 *      CONVENTIONS.md "multipart POST /v1/audio/transcriptions returns JSON {text}") --
 *      bypasses the SDK entirely, so a server-side wire-format regression is caught even if
 *      the SDK's own parsing happens to tolerate it.
 *   4. The same transcription through the SDK's normalized `createProvider(...).transcribe()`,
 *      proving the published SDK's client code actually speaks to the released server.
 *
 * Exits non-zero and prints which check failed on any problem; prints the tested version pair
 * and a GITHUB_STEP_SUMMARY line on success.
 */
import { readFileSync, appendFileSync } from "node:fs";
import { createRequire } from "node:module";
import { join } from "node:path";

const baseUrl = process.env.COMPAT_BASE_URL || "http://127.0.0.1:8000";
const token = process.env.COMPAT_TOKEN;
const serverTag = process.env.COMPAT_SERVER_TAG || "unknown";
const fixturePath = process.env.COMPAT_FIXTURE_WAV;
// Node's module resolution for a bare specifier always walks up from the
// *requiring file's own* directory, never from process.cwd() -- so a plain
// `createRequire(import.meta.url)` here would look for
// node_modules next to this script, not wherever the SDK was actually
// `npm install`ed for the version under test. Resolve explicitly against
// that install directory instead.
const installDir = process.env.COMPAT_SDK_INSTALL_DIR;
if (!token) throw new Error("COMPAT_TOKEN is required");
if (!fixturePath) throw new Error("COMPAT_FIXTURE_WAV is required");
if (!installDir) throw new Error("COMPAT_SDK_INSTALL_DIR is required");

const require = createRequire(join(installDir, "package.json"));
const sdkPackageJson = require("@open-vibe-ai/stt-sdk/package.json");
const sdkVersion = sdkPackageJson.version;
const { createProvider } = require("@open-vibe-ai/stt-sdk");

const authHeaders = { Authorization: `Bearer ${token}` };
const wavBytes = readFileSync(fixturePath);

function fail(step, detail) {
  console.error(`COMPAT FAIL [${step}]: ${detail}`);
  process.exit(1);
}

async function checkHealth() {
  const res = await fetch(`${baseUrl}/health`, { headers: authHeaders });
  if (!res.ok) fail("authenticated /health", `status ${res.status}`);
  console.log("OK: authenticated GET /health");
}

async function checkConfig() {
  const res = await fetch(`${baseUrl}/v1/config`, { headers: authHeaders });
  if (!res.ok) fail("authenticated /v1/config", `status ${res.status}`);
  console.log("OK: authenticated GET /v1/config");
}

// Builds a real multipart/form-data body by hand -- deliberately not using
// the SDK here, so this check exercises the raw OpenDora wire contract
// independent of the SDK's own request-construction code.
function multipartBody(boundary, fields) {
  const parts = [];
  for (const [name, filename, bytes] of fields) {
    parts.push(Buffer.from(`--${boundary}\r\n`));
    parts.push(
      Buffer.from(
        filename
          ? `Content-Disposition: form-data; name="${name}"; filename="${filename}"\r\n\r\n`
          : `Content-Disposition: form-data; name="${name}"\r\n\r\n`,
      ),
    );
    parts.push(Buffer.isBuffer(bytes) ? bytes : Buffer.from(bytes));
    parts.push(Buffer.from("\r\n"));
  }
  parts.push(Buffer.from(`--${boundary}--\r\n`));
  return Buffer.concat(parts);
}

async function checkRawOpenDoraTranscription() {
  const boundary = `compat-check-${Date.now()}`;
  const body = multipartBody(boundary, [["file", "sample.wav", wavBytes]]);
  const res = await fetch(`${baseUrl}/v1/audio/transcriptions`, {
    method: "POST",
    headers: { ...authHeaders, "Content-Type": `multipart/form-data; boundary=${boundary}` },
    body,
  });
  if (!res.ok) fail("raw OpenDora multipart transcription", `status ${res.status}`);
  const json = await res.json();
  if (typeof json.text !== "string" || json.text.trim().length === 0) {
    fail("raw OpenDora multipart transcription", `expected non-empty {text}, got ${JSON.stringify(json)}`);
  }
  console.log(`OK: raw OpenDora multipart transcription -> "${json.text}"`);
}

async function checkSdkTranscription() {
  const provider = createProvider({
    schemaVersion: 1,
    provider: "faster-whisper",
    protocol: "voice-typer-v1",
    transport: "http",
    baseUrl,
    auth: { type: "token", value: token },
  });
  const result = await provider.transcribe({ file: new Uint8Array(wavBytes) });
  if (typeof result.text !== "string" || result.text.trim().length === 0) {
    fail("SDK batch transcription", `expected non-empty text, got ${JSON.stringify(result)}`);
  }
  console.log(`OK: SDK batch transcription via createProvider() -> "${result.text}"`);
}

async function main() {
  await checkHealth();
  await checkConfig();
  await checkRawOpenDoraTranscription();
  await checkSdkTranscription();

  const summary = `@open-vibe-ai/stt-sdk@${sdkVersion} <-> stt-server ${serverTag}: compatible`;
  console.log(`COMPAT OK: ${summary}`);
  if (process.env.GITHUB_STEP_SUMMARY) {
    appendFileSync(process.env.GITHUB_STEP_SUMMARY, `\n## SDK/Server compatibility\n\n${summary}\n`);
  }
}

main().catch((err) => {
  console.error("COMPAT FAIL: unexpected error");
  console.error(err);
  process.exit(1);
});
