# Muse Web

Muse Web uses the existing provider registry, executor, browser pool and account
fallback. It does not change the Meta Model API provider (`muse`).

- Each chat, image or video request uses a fresh thread. Chat history comes from
  the request's messages; unrelated requests never share a conversation.
- After a successful chat, one empty /thread/new page stays open for 60 seconds.
  The next chat on that account reuses it. A page that already has a conversation
  is discarded. Image and video requests still close their page.
- Browser contexts stay pooled. Rotated session cookies stay in memory until
  their context closes; replacing credentials selects a fresh context.
- A successful session check is reused for up to 60 seconds in that context.
  Reuse reads current browser cookies and does not extend the check's expiry.
  A login screen invalidates the check and retries with HTTP session validation.
- One request runs per connection, including the full lifetime of a stream or
  image batch. A busy connection returns `MUSE_WEB_BUSY` with HTTP 429; normal
  fallback tries another account without recording a cooldown or disabling it.
- Client cancellation closes the page and releases the slot. Active contexts
  are protected from idle shutdown and eviction.
- HTTP session renewal has a 15-second deadline per attempt. The web client wakes
  the workspace using browser authentication; the executor skips a duplicate
  HTTP wake. A second HTTP 401 or 403 after the 3-second retry confirms expiration.
  A phrase in the reply does not disable the account. Disable happens only when
  a fresh Usage panel shows weekly use at 100% and additional tokens at 0.
- Chat polls every 120-600 ms and finishes after 600 ms of unchanged text with
  generation stopped, without waiting an extra polling interval. A pause while
  generating is not a successful completion.

Slots are local to one gateway process. Multiple gateway processes would need
shared leases. There is no queue. The warm page is an empty thread, not a
conversation cache.

## Upstream

Branch `muse` on `Lucas-0911/9router` holds these changes. `master` stays the
same commit as `decolua/9router` `master`, so a fork sync does not rewrite Muse
Web. 9router's listen port is unchanged.

```sh
git fetch upstream
git merge upstream/master
```

## Checks

Run from `tests/`:

```sh
./node_modules/.bin/vitest run unit/muse-*.test.js unit/browser-pool-active.test.js
```

The local slow-first-token simulation reduces DOM reads from 86 to 22. A Chrome
fixture confirms that chat snapshots skip attachment scans and full-body text
reads when a reply is present.

## Latency Check (2026-11-09)

Two sequential live requests per version used the same account, headless Chrome,
fresh threads, and the same one-word prompt. Each version started a fresh browser.

| Measurement | Before | After |
| --- | ---: | ---: |
| First request: prompt submitted | 4.45 s | 4.29 s |
| First request: total | 18.31 s | 15.00 s |
| Second request: prompt submitted | 2.04 s | 1.24 s |
| Second request: first content | 6.76 s | 4.21 s |
| Second request: total | 8.57 s | 5.17 s |

The second request skips 0.82 s of measured session/wake HTTP calls. A controlled
completion fixture reduces the final stability wait from 840 ms to 600 ms.
Upstream generation varies: these small samples do not establish a general
latency guarantee or attribute the entire total-time difference to this change.
Live RAM usage and load across multiple accounts were not measured.
