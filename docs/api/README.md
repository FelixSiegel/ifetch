# iFetch API Collection & Reference

1. Run all tests against a running iFetch server

```bash
cd docs/api
bru run -r --env Local
```

Or using `npx` without any permanent installation:

```bash
cd docs/api
npx @usebruno/cli run -r --env Local
```

2. Run a specific folder

```bash
cd docs/api
bru run Catalog -r --env Local
bru run Downloads -r --env Local
```

## Endpoint Directory

### Catalog (`Catalog/`)

- `GET /api/search?q={query}`: Search manga by keyword.
- `GET /api/popular?page={page}`: List popular manga.
- `GET /api/latest?page={page}`: List recently updated manga.
- `GET /api/new?page={page}`: List newly cataloged manga.

### Manga Details & Chapters (`Manga/`)

- `GET /api/manga/{id}`: Full manga details (synopsis, author, genres, cover).
- `GET /api/manga/{id}/chapters?filter={all|downloaded|missing}`: Scraped chapter list with local CBZ download status.

### Downloads (`Downloads/`)

- `POST /api/manga/{id}/download`: Queue background download with optional selective range (returns `202 Accepted`).
- `GET /api/manga/{id}/download`: Live progress telemetry for a specific manga.
- `DELETE /api/manga/{id}/download`: Cancel queued/active downloads for a manga.
- `GET /api/downloads`: Global download queue overview.

### Reader & Streaming (`Reader/`)

- `GET /api/chapter/{id}::{number}/pages`: List image URLs inside a downloaded CBZ.
- `GET /api/image/{id}::{number}/{filename}`: Stream a page image with in-memory caching and prefetching.

### System (`System/`)

- `GET /health`: Health check probe (`200 OK`).
