import express, { type Request, type Response } from "express";

import { greet } from "../shared/greet.js";

const app = express();

// Built client assets (the `:client` target); override with STATIC_DIR.
app.use(express.static(process.env.STATIC_DIR ?? "dist/client"));

app.get("/api/hello/:name", (req: Request, res: Response) => {
  res.json({ message: greet({ name: req.params.name }) });
});

const port = Number(process.env.PORT ?? 3000);
app.listen(port, () => console.log(`listening on :${port}`));
