# hive e2e smoke tests

Playwright smoke suite for hive-ui (SSR) and hive-api.

## Prerequisites

- hive-api on `http://127.0.0.1:7878` (build from this branch for inline-task coverage)
- hive-ui on `http://127.0.0.1:8091` (`cargo leptos build` + run `hive-ui` with `LEPTOS_SITE_ROOT` set)

Override URLs:

```powershell
$env:HIVE_API_URL = "http://127.0.0.1:7879"
$env:HIVE_UI_URL = "http://127.0.0.1:8091"
```

## Run

```powershell
cd e2e
npm install
npx playwright install chromium
npm test
```

The API inline-task test skips automatically when the running hive-api is an older build (404 on `/journal/{id}/tasks`).
