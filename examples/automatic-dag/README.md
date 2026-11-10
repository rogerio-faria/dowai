# Offline DAG example

From the repository root, run:

```powershell
cargo run -p dowai-cli -- validate .\examples\automatic-dag --json
cargo run -p dowai-cli -- plan faturamento.sql --project-dir .\examples\automatic-dag --json
cargo run -p dowai-cli -- plan --manual faturamento.sql stg_vendas.sql --project-dir .\examples\automatic-dag --json
```

The SQL is only inspected as text; no database connection or SQL execution
takes place.
