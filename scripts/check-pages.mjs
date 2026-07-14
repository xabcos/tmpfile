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
  .map(([, attributes]) => ({
    attributes,
    src: attributes.match(/\bsrc=["']([^"']+)["']/i)?.[1],
  }))
  .filter(({ src }) => Boolean(src));
const expectedExternalScripts = [
  "/config.js",
  "https://card.xairouter.com/embed.js",
];
if (
  externalScripts.length !== expectedExternalScripts.length ||
  externalScripts.some(({ src }, index) => src !== expectedExternalScripts[index])
) {
  throw new Error(`pages/index.html external scripts must be: ${expectedExternalScripts.join(", ")}`);
}
if (html.indexOf('src="/config.js"') > html.indexOf("<script>")) {
  throw new Error("/config.js must load before inline application scripts");
}

const cardScriptAttributes = externalScripts[1].attributes;
const readAttribute = (attributes, name) => {
  const escapedName = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return attributes.match(new RegExp(`\\b${escapedName}\\s*=\\s*(["'])(.*?)\\1`, "i"))?.[2];
};
const expectedCardAttributes = {
  integrity: "sha384-WojoUxLlNhOt2Z3O9xjaac3XFCIcDFkk2dSJ8vyx3b1gH40vOod4AhrKRfH1r54j",
  crossorigin: "anonymous",
  referrerpolicy: "no-referrer",
  "data-feed": "feed_d949561dd4914f6e94179f4f7c1f206a",
  "data-mode": "full",
  "data-position": "right",
  "data-interval": "3000",
};
if (!/(?:^|\s)async(?:\s|$)/i.test(cardScriptAttributes)) {
  throw new Error("the card embed script must load asynchronously");
}
for (const [name, value] of Object.entries(expectedCardAttributes)) {
  if (readAttribute(cardScriptAttributes, name) !== value) {
    throw new Error(`the card embed script must set ${name}="${value}"`);
  }
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

const csp = headers.match(/^\s*Content-Security-Policy:\s*(.+)$/m)?.[1];
const cspDirectives = new Map(
  (csp || "").split(";").map((directive) => {
    const [name, ...sources] = directive.trim().split(/\s+/);
    return [name, sources];
  }),
);
for (const [directive, source] of [
  ["script-src", "https://card.xairouter.com"],
  ["style-src", "https://card.xairouter.com"],
  ["connect-src", "https:"],
  ["img-src", "https:"],
]) {
  if (!cspDirectives.get(directive)?.includes(source)) {
    throw new Error(`Pages CSP ${directive} must allow ${source} for the card embed`);
  }
}

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
