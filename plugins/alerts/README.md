# Plugin oficial `alerts` (ENT-061)

Alertas por limiar percentual, com destinatários. Plugin **sem efeito externo**: não envia nada.
A entrega é feita pelo chamador (por exemplo, `email.send`) usando as chaves de `plan`.

## Entrypoints

- `alerts.evaluate(value, rules)`: devolve as regras disparadas por `value` (percentual).
- `alerts.plan(value, rules, event_key)`: devolve uma entrega por destinatário e regra disparada, com
  `idempotency_key` estável (`alerts-<sha256>`), `to`, `subject` e `message`.

Regra: `{"id": "supervisor", "threshold_percent": 10, "recipients": ["a@x.test"]}`.

## Regras

- Dispara somente com `valor > limiar`. Em 10% e 15% exatos não há alerta da regra correspondente.
- Regras são independentes: 15,01% dispara as regras de 10% e de 15%.
- Valores aceitos: int, float ou texto decimal com ponto (`"10.01"`). Booleano, NaN, infinito e vírgula são recusados
  com `DOWAI_ALERTS_INPUT_INVALID` (saída 10).
- Quebra de linha em destinatário ou em `event_key` é recusada.

## Retries

`event_key` identifica o evento de negócio (por exemplo, `run_id:linha`). Repetir o mesmo evento gera as mesmas
chaves. Ao enviar com `email.send` e a mesma chave, a outbox do host devolve `DUPLICATE` e não reenvia.

## Exemplo

```bash
dowai plugins run alerts.plan --input '{"value":"15.01","event_key":"run-1:l-1","rules":[
  {"id":"supervisor","threshold_percent":10,"recipients":["s@x.test"]},
  {"id":"gerente","threshold_percent":15,"recipients":["g@x.test"]}]}' --json
```

## Testes

`python3 -I -m unittest discover -s tests/plugins/alerts -p "test_*.py"`
