# Native wrapper client fixture

This fixture uses `kimojio_http2::Client` and `NativeConnection::run`.
It does not drive the core or implement socket reads and writes.
This checkpoint supports client mode only.

```text
interop client REQUEST_JSON RESULT_JSON
```

The JSON interface follows `interop/http2/ADAPTER.md`.
The fixture polls each send future once, in request order, before it schedules concurrent response work.
Uploads use lazy frames of at most 16 KiB.
Receive chunks return to the wrapper before the fixture waits for full completion.
Only an explicit reset action calls `IncomingBody::cancel`.
Neither status 200 nor status 413 cancels an upload.

`NativeConnection::run` remains active alongside the application.
The report records a confirmed close only after the driver returns a documented terminal connection result.
A watchdog requests abort and permits three seconds for driver settlement.
A missing close result stays unconfirmed.

## Public API limits at phase 1

These limits are not protocol-success exceptions.
An unsupported observation produces a diagnostic and a nonzero exit status.
The fixture does not invent retirement outcomes or HTTP/2 error codes.

| Observation | Limit |
| --- | --- |
| Informational responses | The wrapper consumes them internally. Known `/informational/N` requests fail explicitly. The fixture cannot detect informational responses on other routes. |
| Failed upload completion | `completion()` can return `Error::Send` instead of the retirement outcome. The report preserves the source error and actual reset code, but leaves retirement null. |
| Failure before final headers | `send()` errors expose neither an admitted stream ID nor a separate retirement handle. Such reports cannot qualify a completed case. |
| Strict startup gate | The public API has no peer-SETTINGS-ready signal. The fixture has no raw-frame observer or timing-based substitute. Reduced-window upload cases can fail the strict startup scenario. |
| Global trailer order | `HeaderMap` preserves duplicate values, but not the original order across different names. The fixture rejects multi-name trailer sections instead of asserting an unknown order. |

An empty `informational` list does not establish that the peer sent no informational response.
The current fixture does not qualify informational-response behavior.
Failed or unknown retirement never becomes success because receive END_STREAM was present.

## Bounds

The request file limit is 2 MiB.
The fixture permits at most 4,096 requests and 64 active requests.
Each generated body has a 1 GiB logical limit, independent of retained storage.
Paused leases have an 8 MiB capacity limit and an 8,192-fragment limit.
Response trailer metadata has an 8 MiB aggregate limit.
Absent window fields preserve wrapper defaults.

The focused tests cover submission order, producer bounds, schema fields, reset code zero, and unsupported observations.
Socket reports qualify this wrapper executable, not a replacement direct-core executor.
