# dowai-mysql

Adapter MySQL (ENT-034): leitura (consulta, stream em lotes), escrita (staging, append, replace,
upsert por `ON DUPLICATE KEY UPDATE`), transações e capabilities próprias (`MYSQL_CAPABILITIES`).

Testes de contrato contra MySQL real são opt-in:

```
DOWAI_TEST_MYSQL_HOST/_PORT/_USER/_PASSWORD/_DATABASE=... \
  cargo test -p dowai-mysql --test contract -- --ignored
```

Sem transações DDL e sem atomicidade entre bancos; veja `docs/CONFIGURATION.md`.
