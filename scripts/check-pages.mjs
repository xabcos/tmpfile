import { existsSync, readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import vm from "node:vm";

const requiredAssets = [
  "api/contracts/upload.json",
  "pages/index.html",
  "pages/config.js",
  "pages/favicon.ico",
  "pages/_headers",
];

for (const asset of requiredAssets) {
  if (!existsSync(asset)) {
    throw new Error(`missing Pages asset: ${asset}`);
  }
}

const html = readFileSync("pages/index.html", "utf8");
const config = readFileSync("pages/config.js", "utf8");
const headers = readFileSync("pages/_headers", "utf8");
const uploadContract = JSON.parse(readFileSync("api/contracts/upload.json", "utf8"));
const configContext = { window: {} };
vm.runInNewContext(config, configContext, { filename: "pages/config.js" });
const configuredServerUrl = configContext.window.TMPFILE_CONFIG?.serverUrl;
if (typeof configuredServerUrl !== "string") {
  throw new Error("pages/config.js must define TMPFILE_CONFIG.serverUrl");
}
const parsedServerUrl = new URL(configuredServerUrl);
if (
  !["http:", "https:"].includes(parsedServerUrl.protocol) ||
  parsedServerUrl.origin === "null" ||
  parsedServerUrl.username ||
  parsedServerUrl.password ||
  parsedServerUrl.search ||
  parsedServerUrl.hash
) {
  throw new Error("TMPFILE_CONFIG.serverUrl must be an absolute HTTP(S) URL");
}
const scriptTags = [...html.matchAll(/<script([^>]*)>([\s\S]*?)<\/script>/gi)];
const externalScripts = scriptTags
  .map(([, attributes]) => attributes.match(/\bsrc=["']([^"']+)["']/i)?.[1])
  .filter(Boolean);
if (externalScripts.length !== 1 || externalScripts[0] !== "/config.js") {
  throw new Error("pages/index.html may load only the local /config.js script");
}
if (html.indexOf('src="/config.js"') > html.indexOf("<script>")) {
  throw new Error("/config.js must load before inline application scripts");
}

const inlineScripts = scriptTags
  .filter(([, attributes]) => !/\bsrc\s*=/.test(attributes))
  .map(([, , source]) => source);

if (inlineScripts.length === 0) {
  throw new Error("pages/index.html contains no inline application scripts");
}

inlineScripts.forEach((source, index) => {
  new vm.Script(source, { filename: `pages/index.html#script-${index + 1}` });
  const hash = createHash("sha256").update(source).digest("base64");
  if (!headers.includes(`'sha256-${hash}'`)) {
    throw new Error(`CSP hash is missing for inline script ${index + 1}`);
  }
});

if (!headers.includes("Content-Security-Policy:") || !headers.includes("/\n  Cache-Control: no-cache")) {
  throw new Error("pages/_headers must secure and revalidate the root document");
}

const serverUrlExpression = "${serverUrl}";
const contractMarkers = [
  "window.TMPFILE_CONFIG",
  "Authorization",
  "Bearer ${apiKey}",
  `formData.append('${uploadContract.multipart_field}'`,
  `xhr.open('${uploadContract.method}', \`${serverUrlExpression}${uploadContract.path}\`)`,
  ...uploadContract.required_response_fields.map((field) => `response.${field}`),
];

for (const marker of contractMarkers) {
  if (!html.includes(marker) && !config.includes(marker)) {
    throw new Error(`Pages/API contract marker is missing: ${marker}`);
  }
}

console.log(`Pages check passed (${inlineScripts.length} inline scripts validated).`);
