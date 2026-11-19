# Exemplo `local-plugin-override`

Demonstra a substituição de plugin global por plugin local (plano §9.2, §17.5, ENT-085).

## Estrutura

- `profiles.json`: marca o workspace; o escopo global é `plugins/` ao lado dele.
- `plugins/api/`: plugin global `api.call`, sem rede, devolve `origin = "global"`.
- `project/dowai_project.json`: `plugins.allow_local_override = true` e `plugins.allow_network = true`.
- `project/plugins/api/`: plugin local `api.call`, que consulta uma URL HTTP e devolve `origin = "local"`.

## Execução

Na raiz do repositório, com `target/debug/dowai` construído:

```bash
python3 -I examples/local-plugin-override/run_example.py
```

O runner copia o exemplo para diretório temporário, cria o venv do projeto ali, sobe uma API HTTP
falsa em `127.0.0.1` e verifica:

1. `plugins resolve api.call` registra `replaced_origin = global`;
2. `plugins run api.call` executa o plugin local e recebe status 200 da API falsa;
3. com `plugins.allow_local_override = false`, a resolução falha com `DOWAI_POLICY_DENIED`.

O teste automatizado de ambos os exemplos está em `tests/examples/test_examples.py`:

```bash
python3 -I -m unittest discover -s tests/examples -p "test_*.py"
```
