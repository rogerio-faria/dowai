"""Plugin oficial mínimo `rest` (ENT-058, plano §9.5, AC015).

Entrypoints (plugin.json):
- get: GET de uma URL, com autenticação opcional por token.
- post: POST com corpo JSON.
- get_pages: GET paginado por cursor, sem duplicar registros.

Regras:
- 429 é reexecutado com backoff exponencial (ou `Retry-After` numérico, limitado).
- Redirecionamentos não são seguidos, para o token não sair para outro host.
- Timeout por requisição vira `RestError` com código `DOWAI_REST_TIMEOUT`.
- O token é recebido pelo chamador e nunca é devolvido no resultado nem escrito em log.
  Resolução por referência de segredo fica para a tarefa do plugin de e-mail (D-072).
"""

import json
import socket
import time
import urllib.error
import urllib.parse
import urllib.request

MAX_BODY_BYTES = 8 * 1024 * 1024
MAX_ATTEMPTS_LIMIT = 10
MAX_RETRY_AFTER_SECONDS = 60.0
MAX_PAGES_LIMIT = 10000


class RestError(Exception):
    """Falha do plugin com código estável. O código é exposto nos testes e na documentação."""

    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


_OPENER = urllib.request.build_opener(_NoRedirect())


def _check_limits(timeout_seconds, max_attempts, backoff_seconds):
    if not 1 <= float(timeout_seconds) <= 300:
        raise RestError("DOWAI_REST_INPUT_INVALID", "timeout_seconds fora do intervalo 1 a 300")
    if not 1 <= int(max_attempts) <= MAX_ATTEMPTS_LIMIT:
        raise RestError("DOWAI_REST_INPUT_INVALID", "max_attempts fora do intervalo 1 a 10")
    if float(backoff_seconds) < 0:
        raise RestError("DOWAI_REST_INPUT_INVALID", "backoff_seconds não pode ser negativo")


def _headers(headers, token, auth_header, json_body):
    result = {"Accept": "application/json"}
    if headers:
        result.update({str(k): str(v) for k, v in headers.items()})
    if token:
        result[auth_header] = f"Bearer {token}" if auth_header == "Authorization" else token
    if json_body is not None:
        result["Content-Type"] = "application/json"
    return result


def _retry_delay(error, attempt, backoff_seconds):
    retry_after = error.headers.get("Retry-After") if error.headers else None
    if retry_after is not None:
        try:
            return min(max(float(retry_after), 0.0), MAX_RETRY_AFTER_SECONDS)
        except ValueError:
            pass
    return min(float(backoff_seconds) * (2 ** (attempt - 1)), MAX_RETRY_AFTER_SECONDS)


def _send(method, url, headers, body, timeout_seconds, max_attempts, backoff_seconds):
    """Envia a requisição e devolve (status, corpo decodificado, tentativas com 429)."""
    _check_limits(timeout_seconds, max_attempts, backoff_seconds)
    data = None if body is None else json.dumps(body).encode("utf-8")
    rate_limited = 0
    for attempt in range(1, int(max_attempts) + 1):
        request = urllib.request.Request(url, data=data, method=method, headers=headers)
        try:
            with _OPENER.open(request, timeout=float(timeout_seconds)) as response:
                raw = response.read(MAX_BODY_BYTES + 1)
                if len(raw) > MAX_BODY_BYTES:
                    raise RestError("DOWAI_REST_BODY_TOO_LARGE", "resposta acima de 8 MiB")
                status = response.status
                content_type = response.headers.get("Content-Type", "")
        except urllib.error.HTTPError as error:
            if error.code == 429 and attempt < int(max_attempts):
                rate_limited += 1
                time.sleep(_retry_delay(error, attempt, backoff_seconds))
                continue
            if error.code == 429:
                raise RestError("DOWAI_REST_RATE_LIMITED", "429 persistente após as tentativas")
            raise RestError("DOWAI_REST_HTTP_ERROR", f"HTTP {error.code} na requisição")
        except (socket.timeout, TimeoutError) as error:
            raise RestError("DOWAI_REST_TIMEOUT", "tempo limite da requisição excedido") from error
        except urllib.error.URLError as error:
            if isinstance(error.reason, (socket.timeout, TimeoutError)):
                raise RestError("DOWAI_REST_TIMEOUT", "tempo limite da requisição excedido") from error
            raise RestError("DOWAI_REST_CONNECTION_FAILED", "falha de conexão com a API") from error
        if "json" in content_type or raw[:1] in (b"{", b"["):
            try:
                decoded = json.loads(raw.decode("utf-8"))
            except ValueError as error:
                raise RestError("DOWAI_REST_BODY_INVALID", "corpo JSON inválido") from error
        else:
            decoded = raw.decode("utf-8", errors="replace")
        return status, decoded, rate_limited
    raise RestError("DOWAI_REST_RATE_LIMITED", "429 persistente após as tentativas")


def get(
    url: str,
    headers: dict | None = None,
    token: str | None = None,
    auth_header: str = "Authorization",
    timeout_seconds: int = 10,
    max_attempts: int = 3,
    backoff_seconds: float = 1.0,
) -> dict:
    status, body, retries = _send(
        "GET",
        url,
        _headers(headers, token, auth_header, None),
        None,
        timeout_seconds,
        max_attempts,
        backoff_seconds,
    )
    return {"status": status, "body": body, "retries": retries}


def post(
    url: str,
    body: dict | None = None,
    headers: dict | None = None,
    token: str | None = None,
    auth_header: str = "Authorization",
    timeout_seconds: int = 10,
    max_attempts: int = 3,
    backoff_seconds: float = 1.0,
) -> dict:
    status, decoded, retries = _send(
        "POST",
        url,
        _headers(headers, token, auth_header, body if body is not None else {}),
        body if body is not None else {},
        timeout_seconds,
        max_attempts,
        backoff_seconds,
    )
    return {"status": status, "body": decoded, "retries": retries}


def get_pages(
    url: str,
    items_key: str = "data",
    cursor_key: str = "next_cursor",
    cursor_param: str = "cursor",
    max_pages: int = 100,
    include_items: bool = False,
    headers: dict | None = None,
    token: str | None = None,
    auth_header: str = "Authorization",
    timeout_seconds: int = 10,
    max_attempts: int = 3,
    backoff_seconds: float = 1.0,
) -> dict:
    """Lê páginas por cursor até `cursor_key` ser nulo. Cursor repetido interrompe a leitura."""
    if not 1 <= int(max_pages) <= MAX_PAGES_LIMIT:
        raise RestError("DOWAI_REST_INPUT_INVALID", "max_pages fora do intervalo 1 a 10000")
    base_headers = _headers(headers, token, auth_header, None)
    seen_cursors = set()
    items = []
    records = 0
    pages = 0
    retries = 0
    cursor = None
    while True:
        target = url
        if cursor is not None:
            separator = "&" if urllib.parse.urlsplit(url).query else "?"
            target = f"{url}{separator}{urllib.parse.urlencode({cursor_param: cursor})}"
        status, body, page_retries = _send(
            "GET", target, base_headers, None, timeout_seconds, max_attempts, backoff_seconds
        )
        retries += page_retries
        pages += 1
        if not isinstance(body, dict) or not isinstance(body.get(items_key), list):
            raise RestError("DOWAI_REST_BODY_INVALID", f"página sem lista '{items_key}'")
        page_items = body[items_key]
        records += len(page_items)
        if include_items:
            items.extend(page_items)
        next_cursor = body.get(cursor_key)
        if next_cursor is None or next_cursor == "":
            break
        if next_cursor in seen_cursors:
            raise RestError("DOWAI_REST_CURSOR_REPEATED", "cursor repetido; leitura interrompida")
        if pages >= int(max_pages):
            raise RestError("DOWAI_REST_PAGE_LIMIT", "limite de páginas atingido antes do fim")
        seen_cursors.add(next_cursor)
        cursor = next_cursor
    result = {"pages": pages, "records": records, "retries": retries}
    if include_items:
        result["items"] = items
    return result
