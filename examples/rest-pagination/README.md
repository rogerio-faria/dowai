# Exemplo `rest-pagination`

Demonstra o plugin REST com paginação por cursor, reexecução de 429 e sem duplicação de registros
(plano §17.6, ENT-085). Usa uma API HTTP falsa em `127.0.0.1` e o plugin **local** em
`plugins/rest/`, cópia do plugin oficial `rest` v1.0.0 (`plugins/rest/` na raiz do repositório).

## Conteúdo

- `dowai_project.json`: habilita `plugins.allow_network` e `plugins.allow_secrets`.
- `plugins/rest/`: plugin local (escopo `project`); cópia idêntica ao oficial (verificada por teste).
- `fake_api.py`: API falsa com três páginas; a segunda responde 429 uma vez com `Retry-After: 0`.
- `run_example.py`: copia o exemplo para diretório temporário, cria o venv do projeto ali e executa
  `dowai plugins run rest.get_pages` contra a API falsa.

## Execução

Na raiz do repositório, com `target/debug/dowai` construído (`cargo build -p dowai-cli --locked`):

```bash
python3 -I examples/rest-pagination/run_example.py
```

Resultado esperado: 3 páginas, 5 registros, 1 reexecução por 429, itens `1..5` sem duplicação.

## Limitações

- Execução de plugin Python não é sandbox de SO; o bloqueio de rede é cooperativo (ENT-108).
- A API é falsa; não há validação contra serviço real.
