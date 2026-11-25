import { createServer } from "node:http";
import { setTimeout } from "node:timers/promises";

let replacementAttempts = 0;
createServer(async (req, res) => {
  const json = (body, status = 200) => {
    res.writeHead(status, { "content-type": "application/json" });
    res.end(JSON.stringify(body));
  };
  if (req.url === "/local/v1/models" && !req.headers.authorization) {
    return json({ data: [] });
  }
  if (req.url === "/gateway/v1/models") {
    await setTimeout(1000);
    if (req.headers.authorization === "Bearer fixture-original") {
      replacementAttempts = 0;
      return json({ data: [{ id: "original-model" }] });
    }
    if (req.headers.authorization === "Bearer fixture-replacement") {
      replacementAttempts += 1;
      if (replacementAttempts === 1) return json({ error: "Temporarily unavailable" }, 503);
      return json({ data: [{ id: "replacement-model" }, { id: "another-model" }] });
    }
  }
  return json({ error: "Unexpected discovery request" }, 401);
}).listen(36446, "127.0.0.1");
