# adapters-sqlite

SQLite implementation of `domain::Store`. One `SqliteStore` instance owns one
workspace database file: `SqliteStore::open(path, model, dim)` creates the file
if missing, runs migrations, initialises the single-row `meta` table on first
open, and refuses to open a database whose `meta` disagrees with the runtime
embedding model or dimension.

## sqlx offline workflow

Queries use `sqlx::query!`/`query_as!` compile-time checking. The checked-in
`.sqlx/` directory holds the prepared query metadata, so the workspace builds
without a live database or `DATABASE_URL` (offline mode is sqlx's automatic
fallback when `DATABASE_URL` is unset).

After changing any SQL in this crate (queries or `migrations/`), regenerate it
from this directory:

```sh
export DATABASE_URL="sqlite://$PWD/.dev.db"
rm -f .dev.db && sqlx database create && sqlx migrate run --source migrations
cargo sqlx prepare
rm -f .dev.db
unset DATABASE_URL
```

Commit the resulting `.sqlx/` changes. Requires `sqlx-cli`
(`cargo install sqlx-cli --no-default-features --features sqlite`).
