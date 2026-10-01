import express from "express";
import { fileURLToPath } from "node:url";
import path from "node:path";

import { greet } from "./src/greet.js";

const dir = path.dirname(fileURLToPath(import.meta.url));
const app = express();

app.use(express.static(path.join(dir, "dist")));
app.get("/api/hello/:name", (req, res) => res.json({ message: greet(req.params.name) }));

const port = process.env.PORT || 3000;
app.listen(port, () => console.log(`listening on :${port}`));
