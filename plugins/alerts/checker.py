"""Plugin oficial mínimo `alerts` (ENT-061, plano §9.5, §17.5, AC016, AC017).

Entrypoints (plugin.json):
- evaluate: avalia um valor percentual contra regras de limiar e devolve os alertas disparados.
- plan: gera, a partir de `evaluate`, as mensagens de alerta por destinatário, com chave de
  idempotência determinística. A entrega é feita pelo chamador (por exemplo, `email.send`).

Regras:
- Valores são percentuais (`10.01` significa 10,01%). Aceitos: int, float ou texto decimal com ponto.
  Booleanos, NaN, infinito e texto com vírgula são recusados.
- Um alerta dispara somente quando `valor > limiar` (estritamente acima). Exatamente 10% ou 15%
  não dispara a regra de 10% ou de 15%.
- Cada regra é independente: com valor 15,01% as regras de 10% e 15% disparam juntas.
- A chave de idempotência é derivada de `event_key` + regra + destinatário (SHA-256). Um retry do
  mesmo evento gera as mesmas chaves, e a outbox do host devolve `DUPLICATE` sem reenviar.
- Este plugin não tem efeito externo: `side_effects` é falso e nada é enviado aqui.
"""

import hashlib
from decimal import Decimal, InvalidOperation

MAX_RULES = 50
MAX_RECIPIENTS = 20
MAX_ID_CHARS = 80
MAX_EVENT_KEY_CHARS = 100
EXIT_INVALID = 10


class AlertError(Exception):
    """Falha do plugin com código estável."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code
        self.dowai_exit_code = EXIT_INVALID


def _invalid(message):
    return AlertError("DOWAI_ALERTS_INPUT_INVALID", message)


def _decimal(value, field):
    if isinstance(value, bool):
        raise _invalid(f"{field} deve ser numérico")
    if isinstance(value, int):
        number = Decimal(value)
    elif isinstance(value, float):
        number = Decimal(repr(value))
    elif isinstance(value, str):
        try:
            number = Decimal(value)
        except InvalidOperation as error:
            raise _invalid(f"{field} deve ser decimal com ponto") from error
    else:
        raise _invalid(f"{field} deve ser numérico")
    if not number.is_finite():
        raise _invalid(f"{field} deve ser finito")
    return number


def _text(value, field, limit):
    if not isinstance(value, str) or not value.strip():
        raise _invalid(f"{field} deve ser texto não vazio")
    if len(value) > limit:
        raise _invalid(f"{field} acima de {limit} caracteres")
    if "\r" in value or "\n" in value:
        raise _invalid(f"{field} não pode conter quebra de linha")
    return value


def _rules(rules):
    if not isinstance(rules, list) or not 1 <= len(rules) <= MAX_RULES:
        raise _invalid(f"rules deve ser lista de 1 a {MAX_RULES} regras")
    seen = set()
    parsed = []
    for rule in rules:
        if not isinstance(rule, dict):
            raise _invalid("cada regra deve ser um objeto")
        rule_id = _text(rule.get("id"), "id da regra", MAX_ID_CHARS)
        if rule_id in seen:
            raise _invalid(f"id de regra duplicado: {rule_id}")
        seen.add(rule_id)
        threshold = _decimal(rule.get("threshold_percent"), "threshold_percent")
        recipients = rule.get("recipients")
        if isinstance(recipients, str):
            recipients = [recipients]
        if not isinstance(recipients, list) or not 1 <= len(recipients) <= MAX_RECIPIENTS:
            raise _invalid(f"recipients deve ter de 1 a {MAX_RECIPIENTS} destinatários")
        clean = [_text(item, "destinatário", 320) for item in recipients]
        parsed.append({"id": rule_id, "threshold": threshold, "recipients": clean})
    return parsed


def _triggered(value, rules):
    number = _decimal(value, "value")
    return number, [rule for rule in rules if number > rule["threshold"]]


def evaluate(value, rules):
    """Devolve os alertas disparados por `value` (em percentual), na ordem das regras."""
    parsed = _rules(rules)
    number, hits = _triggered(value, parsed)
    return {
        "value_percent": str(number),
        "alerts": [
            {
                "rule_id": rule["id"],
                "threshold_percent": str(rule["threshold"]),
                "recipients": list(rule["recipients"]),
            }
            for rule in hits
        ],
    }


def plan(value, rules, event_key):
    """Gera uma mensagem por destinatário e regra disparada, com chave de idempotência estável."""
    key_base = _text(event_key, "event_key", MAX_EVENT_KEY_CHARS)
    parsed = _rules(rules)
    number, hits = _triggered(value, parsed)
    deliveries = []
    for rule in hits:
        subject = f"Alerta: valor acima de {rule['threshold']}%"
        message = (
            f"O valor {number}% está acima do limiar de {rule['threshold']}% "
            f"(regra {rule['id']})."
        )
        for recipient in rule["recipients"]:
            digest = hashlib.sha256(
                f"{key_base}\x1f{rule['id']}\x1f{recipient}".encode("utf-8")
            ).hexdigest()
            deliveries.append(
                {
                    "idempotency_key": f"alerts-{digest}",
                    "to": recipient,
                    "subject": subject,
                    "message": message,
                }
            )
    return {"value_percent": str(number), "deliveries": deliveries}
