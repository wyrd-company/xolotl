---
xolotl: minor
---

Policies can set rate limits: requests and tokens per minute and per day, counted per key or shared by the policy. A request over a limit gets 429 with retry-after in the endpoint's error shape. Consumption shows in `xolotl keys list`, the admin API, the web UI and metrics.
