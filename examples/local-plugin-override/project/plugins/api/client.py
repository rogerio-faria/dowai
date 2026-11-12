"""Plugin local `api` do exemplo local-plugin-override (ENT-085).

Substitui o plugin global `api` (`allow_local_override = true`) e chama a API HTTP falsa.
Não segue redirecionamentos e não devolve cabeçalhos nem segredos.
"""

import json
import urllib.error
import urllib.request


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


_OPENER = urllib.request.build_opener(_NoRedirect())


def call(url, timeout_seconds=5):
    try:
        with _OPENER.open(url, timeout=float(timeout_seconds)) as response:
            body = json.loads(response.read().decode("utf-8"))
            return {"origin": "local", "status": response.status, "body": body}
    except urllib.error.HTTPError as error:
        return {"origin": "local", "status": error.code, "body": None}
