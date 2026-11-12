# Plugin oficial `rest` (v1.0.0)

Plugin mínimo de API HTTP (plano §9.5, ENT-058, AC015). Para usar, copie esta pasta para
`<projeto>/plugins/rest/` (escopo `project`) ou `<workspace>/plugins/rest/` (escopo `global`).

## Métodos

| Alvo | Função | Uso |
|---|---|---|
| `rest.get` | `client:get` | GET de uma URL; devolve `status`, `body` e `retries`. |
| `rest.post` | `client:post` | POST com corpo JSON; devolve `status`, `body` e `retries`. |
| `rest.get_pages` | `client:get_pages` | GET paginado por cursor; devolve `pages`, `records`, `retries` e, com `include_items`, `items`. |

Todas as chamadas exigem `idempotency_key` (o manifesto marca efeitos externos), mesmo as leituras.

## Parâmetros principais

- `url` (obrigatório), `headers`, `token` (valor já resolvido pelo chamador), `auth_header` (padrão `Authorization`, com `Bearer`).
- `timeout_seconds` (1 a 300), `max_attempts` (1 a 10), `backoff_seconds` (exponencial; `Retry-After` numérico até 60 s).
- `get_pages`: `items_key` (padrão `data`), `cursor_key` (padrão `next_cursor`), `cursor_param` (padrão `cursor`), `max_pages` (padrão 100).

## Regras

- Só 429 é reexecutado. Após o limite de tentativas, a falha é permanente.
- Redirecionamentos não são seguidos. Respostas acima de 8 MiB são recusadas.
- Cursor repetido ou limite de páginas atingido interrompe a leitura com erro.
- Timeout do manifesto (`timeout_seconds` = 120) vira `DOWAI_PLUGIN_TIMEOUT` no host.
  Um timeout por requisição dentro do plugin vira `DOWAI_PLUGIN_FAILED`, porque o worker não transporta códigos de erro de dentro do Python.

## Permissões e limitações

- Manifesto declara `network: ["rest_api"]` e `secrets: ["rest_token"]`. A política precisa permitir `plugins.allow_network` e `plugins.allow_secrets`.
- Segredo: nesta fase o token chega como valor pelo chamador. Resolver referência de segredo fica para o plugin de e-mail (ENT-059, D-072).
- Bloqueio de rede é cooperativo (hook de auditoria do worker), não sandbox de SO (ENT-108).

## Testes

- `python3 -I -m unittest discover -s tests/plugins/rest -p "test_*.py"` (API falsa local).
- `cargo test -p dowai-runtime --test plugin_rest` (execução pelo worker real).
