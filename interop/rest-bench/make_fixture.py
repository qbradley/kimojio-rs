#!/usr/bin/env python3
"""Generate the single deterministic REST-shaped fixture shared by both servers."""
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent / "fixtures"
items = [{"sku": f"SKU-{i:04}", "quantity": i % 3 + 1} for i in range(1, 21)]
request = {
    "customer": {"id": "customer-1042", "tier": "standard"},
    "currency": "USD",
    "shipping": {"postal_code": "98109", "country": "US"},
    "items": items,
    "coupon": "SUMMER10",
    "client_context": {"channel": "web", "locale": "en-US", "cart_version": 7},
}
lines = [dict(item, unit_price_cents=1999 + i * 100,
              line_total_cents=(1999 + i * 100) * item["quantity"],
              available=True, warehouse="us-west-2", estimated_ship_date="2026-09-24")
         for i, item in enumerate(items)]
response = {
    "quote_id": "quote-20260923-001042", "customer_id": "customer-1042",
    "currency": "USD", "lines": lines,
    "totals": {"subtotal_cents": sum(x["line_total_cents"] for x in lines),
               "shipping_cents": 599, "tax_cents": 8731},
    "expires_at": "2026-09-23T13:00:00Z", "status": "ready",
}
manifest = {
    "method": "POST", "path": "/v1/quotes",
    "request_headers": [
        ["host", "rest-bench.local"], ["content-type", "application/json"],
        ["accept", "application/json"], ["user-agent", "rest-comparison/1.0"],
        ["authorization", "Bearer benchmark-only-not-a-credential-" + "0123456789abcdef" * 4],
        ["x-request-id", "8af106e2-b569-4c6f-81f4-bb1400001042"],
        ["x-tenant-id", "benchmark-company"],
        ["traceparent", "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"],
        ["cookie", "session=benchmark-only-session; preferences=locale-en-US; cart=quote-1042-version-7"],
        ["accept-language", "en-US,en;q=0.9"], ["x-client-version", "2026.09"],
    ],
    "response_headers": [
        ["content-type", "application/json"], ["cache-control", "no-store"],
        ["date", "Wed, 23 Sep 2026 12:00:00 GMT"],
        ["x-request-id", "8af106e2-b569-4c6f-81f4-bb1400001042"],
        ["x-service-version", "1.0"], ["x-content-type-options", "nosniff"],
        ["x-region", "us-west-2"], ["x-ratelimit-remaining", "9999"],
    ],
}
ROOT.mkdir(exist_ok=True)
for name, value in [("request", request), ("response", response)]:
    data = (json.dumps(value, separators=(",", ":")) + "\n").encode()
    (ROOT / f"{name}.json").write_bytes(data)
    manifest[f"{name}_bytes"] = len(data)
(ROOT / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
print(json.dumps({k: v for k, v in manifest.items() if k.endswith("_bytes")}))
