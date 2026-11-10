# dowai-postgres

Initial synchronous PostgreSQL adapter for DOWAI. TLS uses native TLS with full
certificate and hostname verification by default. Passwords are read at
connection time from the environment variable named in `PgProfile`; the
password itself is not stored in the profile or included in adapter errors.

`execute` and `query` accept PostgreSQL parameters separately from SQL text.
`test_connection` sends only `SELECT 1`.

## Optional live smoke test

The smoke test is ignored in normal test runs. To opt in, set these variables
for a dedicated PostgreSQL instance:

- `DOWAI_TEST_PG_HOST`
- `DOWAI_TEST_PG_PORT`
- `DOWAI_TEST_PG_USER`
- `DOWAI_TEST_PG_DATABASE`
- `DOWAI_TEST_PG_PASSWORD`

Set those variables in the test process (for example, in PowerShell, use
`$env:DOWAI_TEST_PG_HOST = "<host>"`, `$env:DOWAI_TEST_PG_PORT = "5432"`,
`$env:DOWAI_TEST_PG_USER = "<user>"`,
`$env:DOWAI_TEST_PG_DATABASE = "<database>"`, and
`$env:DOWAI_TEST_PG_PASSWORD = "<password>"`), then run:

```powershell
cargo test -p dowai-postgres --test smoke -- --ignored --exact live_connection_executes_read_only_select_one
```

The test only opens a connection and runs `SELECT 1`; it makes no writes and
does not create or drop databases. This live end-to-end test has not been run
or verified as part of this implementation.
