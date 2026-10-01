// `npm run check`: fails if a feature that the main app doesn't use is
// broken, and records what it saw in out/check.json.

"use strict";

const {execSync} = require("child_process");
const fs = require("fs");

if (process.env.GREETING !== "hello") {
  throw new Error(`npm_build's env didn't reach the script (GREETING=${process.env.GREETING})`);
}

const result = {
  // A scoped package's bin (`ni` from @antfu/ni) is on PATH.
  ni: execSync("ni --version", {encoding: "utf8"}).trim().split("\n")[0],
  // @aws-cdk/cloud-assembly-schema loads jsonschema and semver, which are
  // bundled in its tarball rather than listed as packages of their own.
  cloudAssemblySchema: require("@aws-cdk/cloud-assembly-schema").Manifest.version(),
  // The same on every machine: npm_build sets TZ=UTC.
  epoch: new Date(0).toString(),
};

fs.mkdirSync("out", {recursive: true});
fs.writeFileSync("out/check.json", JSON.stringify(result, null, 2) + "\n");
